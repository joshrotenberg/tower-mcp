//! Get user profile tool

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, Error, Tool, ToolBuilder,
    extract::{Json, State},
};

use crate::state::AppState;

/// Input for getting user info
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UserInput {
    /// GitHub username
    username: String,
}

pub fn build(state: Arc<AppState>) -> Tool {
    ToolBuilder::new("get_user")
        .description("Get a crates.io user's profile information by their GitHub username.")
        .read_only()
        .idempotent()
        .extractor_handler_typed::<_, _, _, UserInput>(
            state,
            |State(state): State<Arc<AppState>>, Json(input): Json<UserInput>| async move {
                let user = state
                    .client
                    .user(&input.username)
                    .await
                    .map_err(|e| Error::tool(format!("Crates.io API error: {}", e)))?;

                let mut output = format!("# User: {}\n\n", user.login);

                if let Some(name) = &user.name {
                    output.push_str(&format!("**Name:** {}\n\n", name));
                }

                output.push_str(&format!("**GitHub:** https://github.com/{}\n", user.login));

                output.push_str(&format!("**Profile:** {}\n", user.url));

                if let Some(avatar) = &user.avatar {
                    output.push_str(&format!("**Avatar:** {}\n", avatar));
                }

                Ok(CallToolResult::text(output))
            },
        )
        .build()
        .expect("valid tool")
}
