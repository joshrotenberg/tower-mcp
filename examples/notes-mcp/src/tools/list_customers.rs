//! List all customers tool — FT.SEARCH idx:customers *

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, Tool, ToolBuilder,
    extract::{Json, State},
};

use crate::state::{AppState, Customer, CustomerSummary, is_json_output, parse_ft_search};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListCustomersInput {
    /// Output format: "markdown" (default) or "json"
    #[serde(default)]
    output_format: Option<String>,
}

pub fn build(state: Arc<AppState>) -> Tool {
    ToolBuilder::new("list_customers")
        .description(
            "List all customers with their note counts. \
             No parameters required — returns every customer in the system.",
        )
        .read_only()
        .idempotent()
        .extractor_handler(
            state,
            |State(state): State<Arc<AppState>>, Json(input): Json<ListCustomersInput>| async move {
                let mut conn = state.conn();

                // Get all customers
                let values: Vec<redis::Value> = redis::cmd("FT.SEARCH")
                    .arg("idx:customers")
                    .arg("*")
                    .arg("RETURN")
                    .arg("1")
                    .arg("$")
                    .arg("LIMIT")
                    .arg("0")
                    .arg("100")
                    .query_async(&mut conn)
                    .await
                    .map_err(|e| tower_mcp::Error::tool(format!("Redis search error: {e}")))?;

                let (total, rows) = parse_ft_search(values)
                    .map_err(|e| tower_mcp::Error::tool(format!("Parse error: {e}")))?;

                if rows.is_empty() {
                    if is_json_output(&input.output_format) {
                        return Ok(CallToolResult::text("[]"));
                    }
                    return Ok(CallToolResult::text("No customers found."));
                }

                let mut summaries = Vec::with_capacity(rows.len());

                for (_key, json_str) in &rows {
                    let customer: Customer = serde_json::from_str(json_str)
                        .map_err(|e| tower_mcp::Error::tool(format!("JSON parse error: {e}")))?;

                    // Get note count for this customer
                    let note_values: Vec<redis::Value> = redis::cmd("FT.SEARCH")
                        .arg("idx:notes")
                        .arg(format!(
                            "@customerId:{{{}}}",
                            crate::state::escape_tag(&customer.id)
                        ))
                        .arg("LIMIT")
                        .arg("0")
                        .arg("0")
                        .query_async(&mut conn)
                        .await
                        .map_err(|e| tower_mcp::Error::tool(format!("Redis search error: {e}")))?;

                    let note_count = match note_values.first() {
                        Some(redis::Value::Int(n)) => *n as usize,
                        _ => 0,
                    };

                    summaries.push(CustomerSummary {
                        customer,
                        note_count,
                    });
                }

                if is_json_output(&input.output_format) {
                    let json = serde_json::to_string_pretty(&summaries)
                        .map_err(|e| tower_mcp::Error::tool(format!("JSON error: {e}")))?;
                    return Ok(CallToolResult::text(json));
                }

                let mut output = format!("{total} customer(s):\n\n");

                for s in &summaries {
                    let c = &s.customer;
                    output.push_str(&format!(
                        "- **{}** ({}) — {} at {}, tier: {} — {} note(s)\n",
                        c.name, c.id, c.role, c.company, c.tier, s.note_count
                    ));
                }

                Ok(CallToolResult::text(output))
            },
        )
        .build()
}
