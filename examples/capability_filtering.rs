//! Capability filtering example - name-based and role-based access control
//!
//! This example demonstrates two approaches to capability filtering:
//!
//! ## 1. Name-Based Filtering
//! Capabilities prefixed with `admin_` or `internal_` are hidden by default.
//! This is useful for:
//! - Hiding administrative functions from regular users
//! - Hiding internal/debugging tools from production clients
//! - Creating tiered access based on capability naming conventions
//!
//! ## 2. Session-Based Role Filtering
//! The filter functions receive `&SessionState` which supports `get::<T>()` for
//! retrieving typed data stored in the session. This enables dynamic role-based
//! filtering based on auth claims, JWT tokens, user roles, etc.
//!
//! In a real application, auth middleware would validate credentials and store
//! claims in the session using `session.insert(UserClaims { ... })`. The filter
//! functions then check these claims to decide visibility.
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
    PromptBuilder, Resource, ResourceBuilder, SessionState, Tool, ToolBuilder,
};

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoInput {
    message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UserIdInput {
    user_id: String,
}

// === SESSION-BASED AUTH CLAIMS ===
//
// In a real application, auth middleware would:
// 1. Validate credentials (JWT, API key, OAuth token, etc.)
// 2. Extract claims from the validated token
// 3. Store claims in the session: `session.insert(UserClaims { ... })`
//
// The filter functions then retrieve these claims to make access decisions.

/// User claims that would be stored in the session by auth middleware.
#[derive(Debug, Clone)]
struct UserClaims {
    #[allow(dead_code)]
    user_id: String,
    role: UserRole,
}

/// User roles for access control.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
enum UserRole {
    /// Regular user - no admin access
    User,
    /// Admin user - can access admin_ prefixed capabilities
    Admin,
    /// Internal/developer - can access both admin_ and internal_ prefixed capabilities
    Developer,
}

// === FILTER FUNCTIONS ===

/// Check if a capability should be visible based on name prefix and session claims.
///
/// This demonstrates the recommended pattern:
/// 1. Check name-based rules (admin_, internal_ prefixes)
/// 2. If restricted, check session claims for elevated access
fn is_visible(session: &SessionState, name: &str) -> bool {
    let is_admin = name.starts_with("admin_");
    let is_internal = name.starts_with("internal_");

    if !is_admin && !is_internal {
        // Public capability - always visible
        return true;
    }

    // Check session for user claims
    if let Some(claims) = session.get::<UserClaims>() {
        match claims.role {
            UserRole::Developer => true,     // Developers see everything
            UserRole::Admin => !is_internal, // Admins see admin_ but not internal_
            UserRole::User => false,         // Users see neither
        }
    } else {
        // No claims in session - treat as anonymous, hide restricted
        false
    }
}

fn is_visible_tool(session: &SessionState, tool: &Tool) -> bool {
    is_visible(session, tool.name())
}

fn is_visible_resource(session: &SessionState, resource: &Resource) -> bool {
    is_visible(session, resource.name())
}

fn is_visible_prompt(session: &SessionState, prompt: &Prompt) -> bool {
    is_visible(session, prompt.name())
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

    // === ADMIN TOOLS (visible to Admin and Developer roles) ===

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

    // === INTERNAL TOOLS (visible only to Developer role) ===

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
        })
        .build();

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
             prefixed with 'admin_' or 'internal_' require elevated permissions.",
        )
        // Public tools
        .tool(echo)
        .tool(get_time)
        // Admin tools (filtered by role)
        .tool(admin_stats)
        .tool(admin_delete)
        // Internal tools (filtered by role)
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
            CapabilityFilter::new(is_visible_tool)
                // NotFound (default) - don't reveal that hidden tools exist
                .denial_behavior(DenialBehavior::NotFound),
        )
        .resource_filter(
            CapabilityFilter::new(is_visible_resource)
                // Unauthorized - hint that auth might help (useful for debugging)
                .denial_behavior(DenialBehavior::Unauthorized),
        )
        .prompt_filter(CapabilityFilter::new(is_visible_prompt));

    // Create HTTP transport
    let transport = HttpTransport::new(mcp_router).disable_origin_validation();

    // Serve
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    tracing::info!("Starting capability filtering example on http://127.0.0.1:3000");
    tracing::info!("");
    tracing::info!("Access control by role:");
    tracing::info!(
        "  Anonymous/User: echo, get_time, readme, api_reference, greeting, code_review"
    );
    tracing::info!(
        "  Admin: + admin_get_stats, admin_delete_user, admin_system_config, admin_debug_assistant"
    );
    tracing::info!("  Developer: + internal_debug_info, internal_metrics, internal_test_prompt");
    tracing::info!("");
    tracing::info!("Note: This example shows role-based filtering. In production, auth middleware");
    tracing::info!("would validate credentials and store UserClaims in the session.");

    axum::serve(listener, transport.into_router()).await?;

    Ok(())
}
