//! Update note tool — JSON.SET per field

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, Tool, ToolBuilder,
    extract::{Json, State},
};

use crate::state::AppState;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateNoteInput {
    /// Note ID (UUID)
    id: String,
    /// New note content
    #[serde(default)]
    content: Option<String>,
    /// New note type: "meeting", "call", "email", or "general"
    #[serde(default)]
    note_type: Option<String>,
    /// New tags (replaces existing tags)
    #[serde(default)]
    tags: Option<Vec<String>>,
}

pub fn build(state: Arc<AppState>) -> Tool {
    ToolBuilder::new("update_note")
        .description(
            "Update a note's content or metadata. Requires the note ID; \
             all other fields are optional — only provided fields are changed.",
        )
        .extractor_handler(
            state,
            |State(state): State<Arc<AppState>>, Json(input): Json<UpdateNoteInput>| async move {
                let mut conn = state.conn();

                let key = format!("note:{}", input.id);

                // Verify note exists
                let exists: Option<String> = redis::cmd("JSON.GET")
                    .arg(&key)
                    .arg("$.id")
                    .query_async(&mut conn)
                    .await
                    .map_err(|e| tower_mcp::Error::tool(format!("Redis error: {e}")))?;

                if exists.is_none() {
                    return Err(tower_mcp::Error::tool(format!(
                        "Note '{}' not found",
                        input.id
                    )));
                }

                let mut updated = Vec::new();

                if let Some(ref content) = input.content {
                    let json_val = serde_json::to_string(content)
                        .map_err(|e| tower_mcp::Error::tool(format!("Serialization error: {e}")))?;
                    redis::cmd("JSON.SET")
                        .arg(&key)
                        .arg("$.content")
                        .arg(&json_val)
                        .query_async::<()>(&mut conn)
                        .await
                        .map_err(|e| tower_mcp::Error::tool(format!("Redis error: {e}")))?;
                    updated.push("content");
                }

                if let Some(ref note_type) = input.note_type {
                    let json_val = serde_json::to_string(note_type)
                        .map_err(|e| tower_mcp::Error::tool(format!("Serialization error: {e}")))?;
                    redis::cmd("JSON.SET")
                        .arg(&key)
                        .arg("$.noteType")
                        .arg(&json_val)
                        .query_async::<()>(&mut conn)
                        .await
                        .map_err(|e| tower_mcp::Error::tool(format!("Redis error: {e}")))?;
                    updated.push("note_type");
                }

                if let Some(ref tags) = input.tags {
                    let json_val = serde_json::to_string(tags)
                        .map_err(|e| tower_mcp::Error::tool(format!("Serialization error: {e}")))?;
                    redis::cmd("JSON.SET")
                        .arg(&key)
                        .arg("$.tags")
                        .arg(&json_val)
                        .query_async::<()>(&mut conn)
                        .await
                        .map_err(|e| tower_mcp::Error::tool(format!("Redis error: {e}")))?;
                    updated.push("tags");
                }

                if updated.is_empty() {
                    return Ok(CallToolResult::text(
                        "No fields provided to update. Pass at least one of: \
                         content, note_type, tags.",
                    ));
                }

                Ok(CallToolResult::text(format!(
                    "Updated note {} — changed: {}",
                    input.id,
                    updated.join(", ")
                )))
            },
        )
        .build()
}
