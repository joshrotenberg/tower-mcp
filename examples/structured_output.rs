//! Structured output example
//!
//! Demonstrates returning typed JSON data from tools using output schemas.
//! When a tool declares an output schema, MCP clients can parse the structured
//! response programmatically rather than relying on plain text.
//!
//! This example shows:
//! - `CallToolResult::json()` for raw JSON values
//! - `CallToolResult::from_serialize()` for typed Rust structs
//! - `.output_schema()` on `ToolBuilder` for declaring the output shape
//! - Mixed text + structured responses
//!
//! Run with: cargo run --example structured_output
//!
//! Test interactively or with an MCP client connected via stdio.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tower_mcp::{BoxError, CallToolResult, McpRouter, StdioTransport, ToolBuilder};

// Input types

#[derive(Debug, Deserialize, JsonSchema)]
struct LookupInput {
    /// User ID to look up
    user_id: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchInput {
    /// Search query
    query: String,
    /// Maximum results to return
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    5
}

// Output types -- these define the structured response shape

#[derive(Debug, Serialize, JsonSchema)]
struct UserProfile {
    id: u32,
    name: String,
    email: String,
    role: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SearchResults {
    query: String,
    total: u32,
    items: Vec<SearchItem>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SearchItem {
    title: String,
    score: f64,
    snippet: String,
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    eprintln!("Starting structured output example...");

    // Tool 1: Return a typed struct as structured JSON
    // Uses from_serialize() to convert a Rust type to JSON with structured_content
    let output_schema = serde_json::to_value(schemars::schema_for!(UserProfile))?;
    let get_user = ToolBuilder::new("get_user")
        .description("Look up a user profile by ID. Returns structured JSON.")
        .output_schema(output_schema)
        .handler(|input: LookupInput| async move {
            let profile = UserProfile {
                id: input.user_id,
                name: format!("User {}", input.user_id),
                email: format!("user{}@example.com", input.user_id),
                role: if input.user_id == 1 {
                    "admin"
                } else {
                    "member"
                }
                .to_string(),
            };
            CallToolResult::from_serialize(&profile)
        })
        .build();

    // Tool 2: Return search results with output schema
    let output_schema = serde_json::to_value(schemars::schema_for!(SearchResults))?;
    let search = ToolBuilder::new("search")
        .description("Search for items. Returns structured results with scores.")
        .output_schema(output_schema)
        .handler(|input: SearchInput| async move {
            let items: Vec<SearchItem> = (0..input.limit)
                .map(|i| SearchItem {
                    title: format!("Result {} for '{}'", i + 1, input.query),
                    score: 1.0 - (i as f64 * 0.15),
                    snippet: format!("...matching content for '{}'...", input.query),
                })
                .collect();

            let results = SearchResults {
                query: input.query,
                total: items.len() as u32,
                items,
            };
            CallToolResult::from_serialize(&results)
        })
        .build();

    // Tool 3: Return raw JSON using CallToolResult::json()
    // Useful when the shape is dynamic or you're forwarding external API responses
    let get_stats = ToolBuilder::new("get_stats")
        .description("Get server statistics as raw JSON.")
        .handler(|()| async move {
            let stats = serde_json::json!({
                "uptime_seconds": 3600,
                "requests_served": 42,
                "active_connections": 3,
                "version": "1.0.0"
            });
            Ok(CallToolResult::json(stats))
        })
        .build();

    let router = McpRouter::new()
        .server_info("structured-output-example", "1.0.0")
        .instructions(
            "This server demonstrates structured output from tools. Try:\n\
             - get_user: returns a typed UserProfile as JSON\n\
             - search: returns SearchResults with scored items\n\
             - get_stats: returns raw JSON statistics",
        )
        .tool(get_user)
        .tool(search)
        .tool(get_stats);

    eprintln!("Server ready. Connect with an MCP client via stdio.");
    StdioTransport::new(router).run().await?;

    Ok(())
}
