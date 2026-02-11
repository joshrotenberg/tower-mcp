//! Add note tool — JSON.SET

use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, Tool, ToolBuilder,
    extract::{Json, State},
};

use crate::state::{AppState, Note};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddNoteInput {
    /// Customer ID to attach the note to (e.g. "c1")
    customer_id: String,
    /// Note content
    content: String,
    /// Note type: "meeting", "call", "email", or "general"
    #[serde(default = "default_note_type")]
    note_type: String,
    /// Tags for categorization
    #[serde(default)]
    tags: Vec<String>,
}

fn default_note_type() -> String {
    "general".to_string()
}

pub fn build(state: Arc<AppState>) -> Tool {
    ToolBuilder::new("add_note")
        .description(
            "Add a new note for a customer. Requires customer_id and content. \
             Optionally specify note_type (meeting/call/email/general) and tags.",
        )
        .extractor_handler(
            state,
            |State(state): State<Arc<AppState>>, Json(input): Json<AddNoteInput>| async move {
                let mut conn = state.conn();

                // Verify customer exists
                let exists: Option<String> = redis::cmd("JSON.GET")
                    .arg(format!("customer:{}", input.customer_id))
                    .arg("$.name")
                    .query_async(&mut conn)
                    .await
                    .map_err(|e| tower_mcp::Error::tool(format!("Redis error: {e}")))?;

                if exists.is_none() {
                    return Err(tower_mcp::Error::tool(format!(
                        "Customer '{}' not found",
                        input.customer_id
                    )));
                }

                let note_id = uuid::Uuid::new_v4().to_string();
                let note = Note {
                    id: note_id.clone(),
                    customer_id: input.customer_id.clone(),
                    content: input.content,
                    note_type: input.note_type,
                    tags: input.tags,
                    created_at: Utc::now().to_rfc3339(),
                };

                let json_str = serde_json::to_string(&note)
                    .map_err(|e| tower_mcp::Error::tool(format!("Serialization error: {e}")))?;

                redis::cmd("JSON.SET")
                    .arg(format!("note:{note_id}"))
                    .arg("$")
                    .arg(&json_str)
                    .query_async::<()>(&mut conn)
                    .await
                    .map_err(|e| tower_mcp::Error::tool(format!("Redis error: {e}")))?;

                Ok(CallToolResult::text(format!(
                    "Created note {note_id} for customer {}.",
                    input.customer_id
                )))
            },
        )
        .build()
}
