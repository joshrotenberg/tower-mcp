//! Get reverse dependencies tool

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{CallToolResult, Error, Tool, ToolBuilder};

use crate::state::AppState;

/// Input for getting reverse dependencies
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReverseDepsInput {
    /// Crate name
    name: String,
}

pub fn build(state: Arc<AppState>) -> Tool {
    ToolBuilder::new("get_reverse_dependencies")
        .description(
            "Get crates that depend on the specified crate (reverse dependencies). \
             Useful for understanding a crate's ecosystem impact.",
        )
        .read_only()
        .idempotent()
        .handler_with_state(
            state,
            |state: Arc<AppState>, input: ReverseDepsInput| async move {
                let response = state
                    .client
                    .crate_reverse_dependencies(&input.name)
                    .await
                    .map_err(|e| Error::tool(format!("Crates.io API error: {}", e)))?;

                let mut output = format!(
                    "# {} - Reverse Dependencies\n\n\
                     {} crates depend on this crate (showing first {}):\n\n",
                    input.name,
                    response.meta.total,
                    response.dependencies.len()
                );

                for dep in &response.dependencies {
                    output.push_str(&format!(
                        "- **{}** v{} ({})\n",
                        dep.crate_version.crate_name, dep.crate_version.num, dep.dependency.req
                    ));
                }

                Ok(CallToolResult::text(output))
            },
        )
        .build()
        .expect("valid tool")
}
