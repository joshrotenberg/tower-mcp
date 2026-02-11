//! Get customer tool — JSON.GET + FT.SEARCH for notes

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, Tool, ToolBuilder,
    extract::{Json, State},
};

use crate::state::{AppState, Customer, Note, parse_ft_search};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetCustomerInput {
    /// Customer ID (e.g. "c1")
    id: String,
}

pub fn build(state: Arc<AppState>) -> Tool {
    ToolBuilder::new("get_customer")
        .description(
            "Get a customer's full profile and their recent notes. \
             Provides a comprehensive view of the customer relationship.",
        )
        .read_only()
        .idempotent()
        .extractor_handler(
            state,
            |State(state): State<Arc<AppState>>, Json(input): Json<GetCustomerInput>| async move {
                let mut conn = state.conn();

                // Get customer profile
                let json_str: Option<String> = redis::cmd("JSON.GET")
                    .arg(format!("customer:{}", input.id))
                    .arg("$")
                    .query_async(&mut conn)
                    .await
                    .map_err(|e| tower_mcp::Error::tool(format!("Redis error: {e}")))?;

                let json_str = json_str.ok_or_else(|| {
                    tower_mcp::Error::tool(format!("Customer '{}' not found", input.id))
                })?;

                let customers: Vec<Customer> = serde_json::from_str(&json_str)
                    .map_err(|e| tower_mcp::Error::tool(format!("JSON parse error: {e}")))?;
                let customer = customers.into_iter().next().ok_or_else(|| {
                    tower_mcp::Error::tool(format!("Customer '{}' not found", input.id))
                })?;

                // Search for their notes
                let values: Vec<redis::Value> = redis::cmd("FT.SEARCH")
                    .arg("idx:notes")
                    .arg(format!("@customerId:{{{}}}", input.id))
                    .arg("RETURN")
                    .arg("1")
                    .arg("$")
                    .arg("SORTBY")
                    .arg("createdAt")
                    .arg("DESC")
                    .arg("LIMIT")
                    .arg("0")
                    .arg("50")
                    .query_async(&mut conn)
                    .await
                    .map_err(|e| tower_mcp::Error::tool(format!("Redis search error: {e}")))?;

                let (_total, rows) = parse_ft_search(values)
                    .map_err(|e| tower_mcp::Error::tool(format!("Parse error: {e}")))?;

                // Format as markdown
                let mut output = format!(
                    "# {} ({})\n\n\
                     - **Company:** {}\n\
                     - **Role:** {}\n\
                     - **Email:** {}\n\
                     - **Tier:** {}\n\n\
                     ## Notes ({} total)\n\n",
                    customer.name,
                    customer.id,
                    customer.company,
                    customer.role,
                    customer.email,
                    customer.tier,
                    rows.len(),
                );

                for (_key, json_str) in &rows {
                    let n: Note = serde_json::from_str(json_str)
                        .map_err(|e| tower_mcp::Error::tool(format!("JSON parse error: {e}")))?;
                    output.push_str(&format!(
                        "### {} — {} ({})\n**Tags:** {}\n\n{}\n\n",
                        n.note_type,
                        n.created_at,
                        n.id,
                        n.tags.join(", "),
                        n.content,
                    ));
                }

                Ok(CallToolResult::text(output))
            },
        )
        .build()
}
