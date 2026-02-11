//! Search notes tool — FT.SEARCH idx:notes

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, Tool, ToolBuilder,
    extract::{Json, State},
};

use crate::state::{AppState, Note, parse_ft_search};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchNotesInput {
    /// Full-text search query (searches note content)
    query: String,
    /// Filter to notes for a specific customer ID (e.g. "c1")
    #[serde(default)]
    customer_id: Option<String>,
    /// Filter by note type: "meeting", "call", "email", "general"
    #[serde(default)]
    note_type: Option<String>,
    /// Filter by tag (e.g. "renewal", "security")
    #[serde(default)]
    tag: Option<String>,
}

pub fn build(state: Arc<AppState>) -> Tool {
    ToolBuilder::new("search_notes")
        .description(
            "Search customer notes by content. Optionally filter by customer ID, \
             note type (meeting/call/email/general), or tag.",
        )
        .read_only()
        .idempotent()
        .extractor_handler(
            state,
            |State(state): State<Arc<AppState>>, Json(input): Json<SearchNotesInput>| async move {
                let mut conn = state.conn();

                // Build query with optional TAG filters
                let mut parts: Vec<String> = Vec::new();

                if input.query != "*" {
                    parts.push(format!("({})", input.query));
                }
                if let Some(ref cid) = input.customer_id {
                    parts.push(format!("@customerId:{{{cid}}}"));
                }
                if let Some(ref nt) = input.note_type {
                    parts.push(format!("@noteType:{{{nt}}}"));
                }
                if let Some(ref tag) = input.tag {
                    parts.push(format!("@tags:{{{tag}}}"));
                }

                let query = if parts.is_empty() {
                    "*".to_string()
                } else {
                    parts.join(" ")
                };

                let values: Vec<redis::Value> = redis::cmd("FT.SEARCH")
                    .arg("idx:notes")
                    .arg(&query)
                    .arg("RETURN")
                    .arg("1")
                    .arg("$")
                    .arg("SORTBY")
                    .arg("createdAt")
                    .arg("DESC")
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
                        "No notes found matching '{}'.",
                        input.query
                    )));
                }

                let mut output = format!("Found {total} note(s):\n\n");

                for (_key, json_str) in &rows {
                    let n: Note = serde_json::from_str(json_str)
                        .map_err(|e| tower_mcp::Error::tool(format!("JSON parse error: {e}")))?;
                    output.push_str(&format!(
                        "### {} — {} ({})\n**Customer:** {} | **Tags:** {}\n\n{}\n\n---\n\n",
                        n.id,
                        n.note_type,
                        n.created_at,
                        n.customer_id,
                        n.tags.join(", "),
                        n.content
                    ));
                }

                Ok(CallToolResult::text(output))
            },
        )
        .build()
}
