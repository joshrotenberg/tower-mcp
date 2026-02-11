//! Search customers tool — FT.SEARCH idx:customers

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, Tool, ToolBuilder,
    extract::{Json, State},
};

use crate::state::{AppState, Customer, parse_ft_search};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchCustomersInput {
    /// Full-text search query (searches name, company, role)
    query: String,
    /// Filter by tier: "enterprise", "startup", or "smb"
    #[serde(default)]
    tier: Option<String>,
}

pub fn build(state: Arc<AppState>) -> Tool {
    ToolBuilder::new("search_customers")
        .description(
            "Search for customers by name, company, or role. \
             Optionally filter by account tier (enterprise, startup, smb).",
        )
        .read_only()
        .idempotent()
        .extractor_handler(
            state,
            |State(state): State<Arc<AppState>>,
             Json(input): Json<SearchCustomersInput>| async move {
                let mut conn = state.conn();

                // Build the FT.SEARCH query
                let text_part = if input.query == "*" {
                    None
                } else {
                    Some(format!("({})", input.query))
                };
                let tier_part = input.tier.as_ref().map(|t| format!("@tier:{{{t}}}"));

                let query = match (text_part, tier_part) {
                    (Some(t), Some(f)) => format!("{t} {f}"),
                    (Some(t), None) => t,
                    (None, Some(f)) => f,
                    (None, None) => "*".to_string(),
                };

                let values: Vec<redis::Value> = redis::cmd("FT.SEARCH")
                    .arg("idx:customers")
                    .arg(&query)
                    .arg("RETURN")
                    .arg("1")
                    .arg("$")
                    .arg("LIMIT")
                    .arg("0")
                    .arg("20")
                    .query_async(&mut conn)
                    .await
                    .map_err(|e| tower_mcp::Error::tool(format!("Redis search error: {e}")))?;

                let (total, rows) = parse_ft_search(values)
                    .map_err(|e| tower_mcp::Error::tool(format!("Parse error: {e}")))?;

                if rows.is_empty() {
                    return Ok(CallToolResult::text(format!(
                        "No customers found matching '{}'.",
                        input.query
                    )));
                }

                let mut output = format!("Found {total} customer(s):\n\n");

                for (_key, json_str) in &rows {
                    let c: Customer = serde_json::from_str(json_str)
                        .map_err(|e| tower_mcp::Error::tool(format!("JSON parse error: {e}")))?;
                    output.push_str(&format!(
                        "- **{}** ({}) — {} at {}, tier: {}\n",
                        c.name, c.id, c.role, c.company, c.tier
                    ));
                }

                Ok(CallToolResult::text(output))
            },
        )
        .build()
}
