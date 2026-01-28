//! Get owners tool

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{CallToolResult, Error, Tool, ToolBuilder};

use crate::state::AppState;

/// Input for getting owners
#[derive(Debug, Deserialize, JsonSchema)]
pub struct OwnersInput {
    /// Crate name
    name: String,
}

pub fn build(state: Arc<AppState>) -> Tool {
    ToolBuilder::new("get_owners")
        .description(
            "Get the owners/maintainers of a crate. Shows GitHub usernames \
             and team memberships.",
        )
        .read_only()
        .idempotent()
        .handler(move |input: OwnersInput| {
            let state = state.clone();
            async move {
                let owners = state
                    .client
                    .crate_owners(&input.name)
                    .await
                    .map_err(|e| Error::tool(format!("Crates.io API error: {}", e)))?;

                let mut output = format!("# {} - Owners\n\n", input.name);

                let (users, teams): (Vec<_>, Vec<_>) = owners
                    .iter()
                    .partition(|o| o.kind.as_deref() == Some("user"));

                if !users.is_empty() {
                    output.push_str("## Users\n\n");
                    for owner in &users {
                        output.push_str(&format!("- **{}**", owner.login));
                        if let Some(name) = &owner.name {
                            output.push_str(&format!(" ({})", name));
                        }
                        output.push('\n');
                    }
                }

                if !teams.is_empty() {
                    output.push_str("\n## Teams\n\n");
                    for owner in &teams {
                        output.push_str(&format!("- **{}**\n", owner.login));
                    }
                }

                Ok(CallToolResult::text(output))
            }
        })
        .build()
        .expect("valid tool")
}
