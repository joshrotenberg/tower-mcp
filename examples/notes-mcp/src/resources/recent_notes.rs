//! Recent notes resource — 10 most recent notes across all customers

use std::sync::Arc;

use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
use tower_mcp::resource::{Resource, ResourceBuilder};

use crate::state::{AppState, Note, parse_ft_search};

pub fn build(state: Arc<AppState>) -> Resource {
    ResourceBuilder::new("notes://recent")
        .name("Recent Notes")
        .description("The 10 most recent notes across all customers")
        .mime_type("text/markdown")
        .handler(move || {
            let state = state.clone();
            async move {
                let mut conn = state.conn();

                let values: Vec<redis::Value> = redis::cmd("FT.SEARCH")
                    .arg("idx:notes")
                    .arg("*")
                    .arg("RETURN")
                    .arg("1")
                    .arg("$")
                    .arg("SORTBY")
                    .arg("createdAt")
                    .arg("DESC")
                    .arg("LIMIT")
                    .arg("0")
                    .arg("10")
                    .query_async(&mut conn)
                    .await
                    .map_err(|e| tower_mcp::Error::tool(format!("Redis search error: {e}")))?;

                let (total, rows) = parse_ft_search(values)
                    .map_err(|e| tower_mcp::Error::tool(format!("Parse error: {e}")))?;

                let mut content = format!(
                    "# Recent Notes\n\n{total} total note(s) in system. Showing most recent:\n\n"
                );

                for (_key, json_str) in &rows {
                    let n: Note = serde_json::from_str(json_str)
                        .map_err(|e| tower_mcp::Error::tool(format!("JSON parse error: {e}")))?;
                    content.push_str(&format!(
                        "### {} — {} ({})\n**Customer:** {} | **Tags:** {}\n\n{}\n\n---\n\n",
                        n.id,
                        n.note_type,
                        n.created_at,
                        n.customer_id,
                        n.tags.join(", "),
                        n.content
                    ));
                }

                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: "notes://recent".to_string(),
                        mime_type: Some("text/markdown".to_string()),
                        text: Some(content),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                })
            }
        })
        .build()
}
