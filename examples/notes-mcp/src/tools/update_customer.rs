//! Update customer tool — JSON.SET per field

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, Tool, ToolBuilder,
    extract::{Json, State},
};

use crate::state::AppState;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateCustomerInput {
    /// Customer ID (e.g. "c1")
    id: String,
    /// New name for the customer
    #[serde(default)]
    name: Option<String>,
    /// New company name
    #[serde(default)]
    company: Option<String>,
    /// New email address
    #[serde(default)]
    email: Option<String>,
    /// New role/title
    #[serde(default)]
    role: Option<String>,
    /// New account tier (enterprise, startup, smb)
    #[serde(default)]
    tier: Option<String>,
}

pub fn build(state: Arc<AppState>) -> Tool {
    ToolBuilder::new("update_customer")
        .description(
            "Update a customer's profile fields. Requires the customer ID; \
             all other fields are optional — only provided fields are changed.",
        )
        .extractor_handler(
            state,
            |State(state): State<Arc<AppState>>,
             Json(input): Json<UpdateCustomerInput>| async move {
                let mut conn = state.conn();

                // Verify customer exists
                let exists: Option<String> = redis::cmd("JSON.GET")
                    .arg(format!("customer:{}", input.id))
                    .arg("$.name")
                    .query_async(&mut conn)
                    .await
                    .map_err(|e| tower_mcp::Error::tool(format!("Redis error: {e}")))?;

                if exists.is_none() {
                    return Err(tower_mcp::Error::tool(format!(
                        "Customer '{}' not found",
                        input.id
                    )));
                }

                let key = format!("customer:{}", input.id);
                let mut updated = Vec::new();

                let fields: Vec<(&str, Option<&String>)> = vec![
                    ("name", input.name.as_ref()),
                    ("company", input.company.as_ref()),
                    ("email", input.email.as_ref()),
                    ("role", input.role.as_ref()),
                    ("tier", input.tier.as_ref()),
                ];

                for (field, value) in fields {
                    if let Some(val) = value {
                        let json_val = serde_json::to_string(val).map_err(|e| {
                            tower_mcp::Error::tool(format!("Serialization error: {e}"))
                        })?;
                        redis::cmd("JSON.SET")
                            .arg(&key)
                            .arg(format!("$.{field}"))
                            .arg(&json_val)
                            .query_async::<()>(&mut conn)
                            .await
                            .map_err(|e| tower_mcp::Error::tool(format!("Redis error: {e}")))?;
                        updated.push(field);
                    }
                }

                if updated.is_empty() {
                    return Ok(CallToolResult::text(
                        "No fields provided to update. Pass at least one of: \
                         name, company, email, role, tier.",
                    ));
                }

                Ok(CallToolResult::text(format!(
                    "Updated customer {} — changed: {}",
                    input.id,
                    updated.join(", ")
                )))
            },
        )
        .build()
}
