//! Tool selection example -- managing large tool surfaces
//!
//! Shows how to organize tools into groups and selectively expose them
//! using existing tower-mcp primitives. This pattern scales to hundreds
//! of tools across multiple domains.
//!
//! Three layers of selection, each narrowing the visible set:
//!
//! 1. **Tool groups** -- organize tools into domain-specific sub-routers,
//!    merge only enabled groups (simulates `--tools users,analytics` CLI flag)
//!
//! 2. **Safety tier** -- annotation-based write protection via `write_guard()`.
//!    Read-only tools always visible; write tools blocked unless opted in.
//!
//! 3. **Allow/deny lists** -- static lists for fine-grained control
//!    (simulates a policy file with `include`/`exclude` directives)
//!
//! Configure via environment variables:
//!
//! ```bash
//! # Default: users + analytics groups, read-only, essentials preset
//! cargo run --example tool_selection
//!
//! # Enable all groups with full write access
//! TOOL_GROUPS=users,billing,analytics READ_ONLY=false cargo run --example tool_selection
//!
//! # Only billing tools, all visible
//! TOOL_GROUPS=billing PRESET=all cargo run --example tool_selection
//! ```
//!
//! Run with: `cargo run --example tool_selection`

use std::collections::HashSet;

use tower_mcp::{
    BoxError, CallToolResult, CapabilityFilter, McpRouter, NoParams, StdioTransport, Tool,
    ToolBuilder,
};

// ---------------------------------------------------------------------------
// Tool groups -- each returns a Vec<Tool> for a domain
// ---------------------------------------------------------------------------

fn user_tools() -> Vec<Tool> {
    vec![
        ToolBuilder::new("list_users")
            .description("List all users")
            .read_only()
            .handler(|_: NoParams| async { Ok(CallToolResult::text("alice, bob, carol")) })
            .build(),
        ToolBuilder::new("get_user")
            .description("Get user details by ID")
            .read_only()
            .handler(|_: NoParams| async {
                Ok(CallToolResult::text(
                    r#"{"id": 1, "name": "alice", "role": "admin"}"#,
                ))
            })
            .build(),
        ToolBuilder::new("create_user")
            .description("Create a new user account")
            .handler(|_: NoParams| async { Ok(CallToolResult::text("User created: dave")) })
            .build(),
        ToolBuilder::new("delete_user")
            .description("Delete a user account")
            .handler(|_: NoParams| async { Ok(CallToolResult::text("User deleted")) })
            .build(),
    ]
}

fn billing_tools() -> Vec<Tool> {
    vec![
        ToolBuilder::new("list_invoices")
            .description("List recent invoices")
            .read_only()
            .handler(|_: NoParams| async {
                Ok(CallToolResult::text("INV-001: $99, INV-002: $149"))
            })
            .build(),
        ToolBuilder::new("get_invoice")
            .description("Get invoice details")
            .read_only()
            .handler(|_: NoParams| async {
                Ok(CallToolResult::text(
                    r#"{"id": "INV-001", "amount": 99, "status": "paid"}"#,
                ))
            })
            .build(),
        ToolBuilder::new("issue_refund")
            .description("Issue a refund for an invoice")
            .handler(|_: NoParams| async { Ok(CallToolResult::text("Refund issued: $99")) })
            .build(),
    ]
}

fn analytics_tools() -> Vec<Tool> {
    vec![
        ToolBuilder::new("query_metrics")
            .description("Query system metrics")
            .read_only()
            .handler(|_: NoParams| async {
                Ok(CallToolResult::text(
                    "requests: 12,450/min, p99: 45ms, errors: 0.1%",
                ))
            })
            .build(),
        ToolBuilder::new("export_report")
            .description("Export a metrics report")
            .handler(|_: NoParams| async {
                Ok(CallToolResult::text(
                    "Report exported to reports/2026-03.csv",
                ))
            })
            .build(),
    ]
}

// ---------------------------------------------------------------------------
// Preset definitions
// ---------------------------------------------------------------------------

/// Essential tools per group -- a curated read-heavy subset.
const ESSENTIALS: &[&str] = &[
    // users
    "list_users",
    "get_user",
    // billing
    "list_invoices",
    "get_invoice",
    // analytics
    "query_metrics",
];

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    // -- Configuration from environment (CLI flags in a real app) --

    let enabled_groups: HashSet<String> = std::env::var("TOOL_GROUPS")
        .unwrap_or_else(|_| "users,analytics".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    let read_only = std::env::var("READ_ONLY")
        .map(|v| v != "false")
        .unwrap_or(true);

    let preset = std::env::var("PRESET").unwrap_or_else(|_| "essentials".to_string());

    eprintln!("Tool selection example");
    eprintln!("  Groups: {:?}", enabled_groups);
    eprintln!("  Read-only: {read_only}");
    eprintln!("  Preset: {preset}");
    eprintln!();

    // -- Layer 1: Group selection --
    // Only register tools from enabled groups. Tools from disabled groups
    // are never registered, saving context tokens in tools/list responses.

    let groups: Vec<(&str, Vec<Tool>)> = vec![
        ("users", user_tools()),
        ("billing", billing_tools()),
        ("analytics", analytics_tools()),
    ];

    let mut router = McpRouter::new()
        .server_info("tool-selection-example", "1.0.0")
        .instructions(
            "API gateway with configurable tool groups. \
             Set TOOL_GROUPS, READ_ONLY, and PRESET env vars to control visibility.",
        );

    let mut registered = Vec::new();
    for (name, tools) in groups {
        if enabled_groups.contains(name) {
            for tool in tools {
                registered.push(tool.name.clone());
                router = router.tool(tool);
            }
        }
    }

    eprintln!("Registered {} tools from enabled groups", registered.len());

    // -- Layers 2+3: Combined filter for safety tier and preset --
    // A single CapabilityFilter composes both concerns.

    let essentials: HashSet<&str> = ESSENTIALS.iter().copied().collect();
    let use_preset = preset == "essentials";

    router = router.tool_filter(CapabilityFilter::new(move |_session, tool: &Tool| {
        // Layer 2: Safety tier
        // Block write tools in read-only mode (read_only_hint tools always pass)
        if read_only {
            let is_read_only = tool.annotations.as_ref().is_some_and(|a| a.read_only_hint);
            if !is_read_only {
                return false;
            }
        }

        // Layer 3: Preset filter
        // In "essentials" mode, only show curated tools
        if use_preset && !essentials.contains(tool.name.as_str()) {
            return false;
        }

        true
    }));

    eprintln!("Server ready. Connect with an MCP client via stdio.");
    StdioTransport::new(router).run().await?;

    Ok(())
}
