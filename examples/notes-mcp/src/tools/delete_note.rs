//! Delete note tool — DEL note:{id}

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, Tool, ToolBuilder,
    extract::{Json, State},
};

use crate::state::AppState;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteNoteInput {
    /// Note ID to delete (UUID)
    id: String,
}

pub fn build(state: Arc<AppState>) -> Tool {
    ToolBuilder::new("delete_note")
        .description("Delete a note by its ID. This action is permanent.")
        .extractor_handler(
            state,
            |State(state): State<Arc<AppState>>, Json(input): Json<DeleteNoteInput>| async move {
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

                redis::cmd("DEL")
                    .arg(&key)
                    .query_async::<()>(&mut conn)
                    .await
                    .map_err(|e| tower_mcp::Error::tool(format!("Redis error: {e}")))?;

                Ok(CallToolResult::text(format!("Deleted note {}.", input.id)))
            },
        )
        .build()
}
