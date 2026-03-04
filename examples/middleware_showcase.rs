//! Middleware showcase: config validation pattern
//!
//! Demonstrates how tower middleware eliminates duplicated validation logic
//! across tool handlers. Instead of checking configuration constraints in
//! every handler, shared guard closures enforce them centrally.
//!
//! # The Problem
//!
//! Without middleware, every tool handler duplicates the same checks:
//!
//! ```rust,ignore
//! async fn handle_insert(config: &Config, input: InsertInput) -> Result<CallToolResult> {
//!     // Duplicated in every write handler (insert, update, delete)
//!     if config.read_only {
//!         return Ok(CallToolResult::error("Server is in read-only mode"));
//!     }
//!     // Duplicated in every handler that takes a table name
//!     if !config.allowed_tables.contains(&input.table) {
//!         return Ok(CallToolResult::error(format!("Table '{}' is not allowed", input.table)));
//!     }
//!     // ... actual logic
//! }
//! ```
//!
//! With 6 tools (3 writes, 5 needing table checks), that's 24+ lines of
//! duplicated validation -- and every new tool must remember to add them.
//!
//! # The Solution
//!
//! Define each concern once as a guard closure, then compose with `.guard()`:
//!
//! ```rust,ignore
//! // Defined once, used on all write tools
//! let write_guard = move |_req: &ToolRequest| { /* check read_only */ };
//!
//! // Defined once, used on all tools that take a table argument
//! let table_guard = move |req: &ToolRequest| { /* check allowed_tables */ };
//!
//! let insert = ToolBuilder::new("insert")
//!     .handler(|input: InsertInput| async move { /* just the business logic */ })
//!     .guard(write_guard.clone())
//!     .guard(table_guard.clone())
//!     .build();
//! ```
//!
//! Guards short-circuit before the handler runs. No validation in handlers.
//!
//! # Middleware Layers
//!
//! | Layer | Scope | Tools |
//! |-------|-------|-------|
//! | `write_guard` | Per-tool guard | insert, update, delete |
//! | `table_guard` | Per-tool guard | describe_table, query, insert, update, delete |
//! | Confirmation guard | Per-tool guard | delete only |
//! | `TimeoutLayer` | Transport (global) | All requests |
//! | `ConcurrencyLimitLayer` | Transport (global) | All requests |
//!
//! Run with: `cargo run --example middleware_showcase`

use std::collections::HashSet;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tower::ServiceBuilder;
use tower::timeout::TimeoutLayer;
use tower_mcp::{
    BoxError, CallToolResult, McpRouter, NoParams, StdioTransport, ToolBuilder, ToolRequest,
};

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct TableInput {
    /// Table name to operate on
    table: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct QueryInput {
    /// Table name to query
    table: String,
    /// Optional filter expression (e.g. "status = 'active'")
    filter: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct InsertInput {
    /// Table name to insert into
    table: String,
    /// JSON data to insert
    data: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateInput {
    /// Table name to update
    table: String,
    /// Filter expression to select rows
    filter: String,
    /// JSON data to set on matching rows
    data: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DeleteInput {
    /// Table name to delete from
    table: String,
    /// Filter expression to select rows
    filter: String,
    /// Must be true to confirm deletion (checked by guard, not handler)
    #[allow(dead_code)]
    confirm: bool,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    eprintln!("Starting middleware showcase (database proxy)...");

    // -- Server configuration --
    let read_only = true;
    let allowed_tables: HashSet<String> = ["users", "orders", "products"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();

    // -- Reusable guard: block writes in read-only mode --
    // Defined once, applied to insert, update, and delete.
    let write_guard = move |_req: &ToolRequest| -> Result<(), String> {
        if read_only {
            Err("Server is in read-only mode".to_string())
        } else {
            Ok(())
        }
    };

    // -- Reusable guard: restrict tables to allowlist --
    // Defined once, applied to every tool that takes a "table" argument.
    let table_guard = {
        let allowed = allowed_tables.clone();
        move |req: &ToolRequest| -> Result<(), String> {
            let table = req.args.get("table").and_then(|v| v.as_str()).unwrap_or("");
            if allowed.contains(table) {
                Ok(())
            } else {
                Err(format!(
                    "Table '{}' is not in the allowlist: {:?}",
                    table,
                    allowed.iter().collect::<Vec<_>>()
                ))
            }
        }
    };

    // -- Tools --

    // list_tables: no guards (always allowed)
    let list_tables = ToolBuilder::new("list_tables")
        .description("List all available tables")
        .handler(move |_input: NoParams| {
            let tables = allowed_tables.clone();
            async move {
                let names: Vec<&str> = tables.iter().map(|s| s.as_str()).collect();
                Ok(CallToolResult::text(format!(
                    "Tables: {}",
                    names.join(", ")
                )))
            }
        })
        .build();

    // describe_table: table guard only
    let describe_table = ToolBuilder::new("describe_table")
        .description("Describe a table's schema")
        .handler(|input: TableInput| async move {
            Ok(CallToolResult::text(format!(
                "Schema for '{}': id (INT), name (TEXT), created_at (TIMESTAMP)",
                input.table
            )))
        })
        .guard(table_guard.clone())
        .build();

    // query: table guard only
    let query = ToolBuilder::new("query")
        .description("Query rows from a table")
        .handler(|input: QueryInput| async move {
            let filter_desc = input.filter.as_deref().unwrap_or("none");
            Ok(CallToolResult::text(format!(
                "3 rows from '{}' (filter: {})",
                input.table, filter_desc
            )))
        })
        .guard(table_guard.clone())
        .build();

    // insert: write guard + table guard
    let insert = ToolBuilder::new("insert")
        .description("Insert a row into a table")
        .handler(|input: InsertInput| async move {
            Ok(CallToolResult::text(format!(
                "Inserted into '{}': {}",
                input.table, input.data
            )))
        })
        .guard(write_guard)
        .guard(table_guard.clone())
        .build();

    // update: write guard + table guard
    let update = ToolBuilder::new("update")
        .description("Update rows in a table")
        .handler(|input: UpdateInput| async move {
            Ok(CallToolResult::text(format!(
                "Updated '{}' where {}: {}",
                input.table, input.filter, input.data
            )))
        })
        .guard(write_guard)
        .guard(table_guard.clone())
        .build();

    // delete: write guard + table guard + confirmation guard
    let delete = ToolBuilder::new("delete")
        .description("Delete rows from a table (requires confirm: true)")
        .handler(|input: DeleteInput| async move {
            Ok(CallToolResult::text(format!(
                "Deleted from '{}' where {}",
                input.table, input.filter
            )))
        })
        .guard(write_guard)
        .guard(table_guard)
        .guard(|req: &ToolRequest| {
            let confirm = req
                .args
                .get("confirm")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if confirm {
                Ok(())
            } else {
                Err("Must set confirm: true to delete rows".to_string())
            }
        })
        .build();

    // -- Router --
    let router = McpRouter::new()
        .server_info("middleware-showcase", "1.0.0")
        .instructions(
            "Database proxy with config-based access control.\n\
             \n\
             This server is in READ-ONLY mode. Write operations (insert, update, delete)\n\
             are blocked by middleware before they reach the handler.\n\
             \n\
             Only these tables are accessible: users, orders, products.\n\
             Requests to other tables are rejected by middleware.",
        )
        .tool(list_tables)
        .tool(describe_table)
        .tool(query)
        .tool(insert)
        .tool(update)
        .tool(delete);

    // -- Transport with global middleware --
    // 30-second timeout and max 5 concurrent requests across all tools.
    eprintln!("Server ready. Connect with an MCP client via stdio.");
    StdioTransport::new(router)
        .layer(
            ServiceBuilder::new()
                .layer(TimeoutLayer::new(Duration::from_secs(30)))
                .concurrency_limit(5)
                .into_inner(),
        )
        .run()
        .await?;

    Ok(())
}
