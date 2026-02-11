//! Resource template for customer profiles with notes
//!
//! Exposes customer info as resources via URI template: notes://customers/{id}

use std::collections::HashMap;
use std::sync::Arc;

use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
use tower_mcp::resource::{ResourceTemplate, ResourceTemplateBuilder};

use crate::state::{AppState, Customer, Note, escape_tag, parse_ft_search};

pub fn build(state: Arc<AppState>) -> ResourceTemplate {
    ResourceTemplateBuilder::new("notes://customers/{id}")
        .name("Customer Profile")
        .description("Get a customer's profile and recent notes by ID")
        .mime_type("text/markdown")
        .handler(move |uri: String, vars: HashMap<String, String>| {
            let state = state.clone();
            async move {
                let id = vars.get("id").cloned().unwrap_or_default();
                let mut conn = state.conn();

                // Get customer profile
                let json_str: Option<String> = redis::cmd("JSON.GET")
                    .arg(format!("customer:{id}"))
                    .arg("$")
                    .query_async(&mut conn)
                    .await
                    .map_err(|e| tower_mcp::Error::tool(format!("Redis error: {e}")))?;

                let json_str = json_str
                    .ok_or_else(|| tower_mcp::Error::tool(format!("Customer '{id}' not found")))?;

                let customers: Vec<Customer> = serde_json::from_str(&json_str)
                    .map_err(|e| tower_mcp::Error::tool(format!("JSON parse error: {e}")))?;
                let customer = customers
                    .into_iter()
                    .next()
                    .ok_or_else(|| tower_mcp::Error::tool(format!("Customer '{id}' not found")))?;

                // Get their notes
                let values: Vec<redis::Value> = redis::cmd("FT.SEARCH")
                    .arg("idx:notes")
                    .arg(format!("@customerId:{{{}}}", escape_tag(&id)))
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

                let mut content = format!(
                    "# {} ({})\n\n\
                     - **Company:** {}\n\
                     - **Role:** {}\n\
                     - **Email:** {}\n\
                     - **Tier:** {}\n\n\
                     ## Notes\n\n",
                    customer.name,
                    customer.id,
                    customer.company,
                    customer.role,
                    customer.email,
                    customer.tier,
                );

                for (_key, json_str) in &rows {
                    let n: Note = serde_json::from_str(json_str)
                        .map_err(|e| tower_mcp::Error::tool(format!("JSON parse error: {e}")))?;
                    content.push_str(&format!(
                        "### {} — {} ({})\n**Tags:** {}\n\n{}\n\n",
                        n.note_type,
                        n.created_at,
                        n.id,
                        n.tags.join(", "),
                        n.content,
                    ));
                }

                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: Some("text/markdown".to_string()),
                        text: Some(content),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                })
            }
        })
}
