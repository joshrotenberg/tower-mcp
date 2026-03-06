//! Stdio server that uses sampling, elicitation, and progress.
//!
//! This server is designed to pair with the `client_handler` example, which
//! spawns it as a subprocess and handles its server-to-client requests.
//!
//! Tools:
//! - `summarize`: Asks the client to summarize text via sampling (LLM request)
//! - `confirm_delete`: Asks the client for confirmation via elicitation
//! - `slow_task`: Reports progress over multiple steps
//!
//! Run standalone: cargo run --example sampling_server
//! Pair with client: cargo run --example client_handler

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, McpRouter, StdioTransport, ToolBuilder,
    extract::{Context, Json},
    protocol::{
        ContentRole, CreateMessageParams, ElicitAction, ElicitFormParams, ElicitFormSchema,
        SamplingContent, SamplingContentOrArray, SamplingMessage,
    },
};

#[derive(Debug, Deserialize, JsonSchema)]
struct SummarizeInput {
    /// The text to summarize
    text: String,
    /// Maximum tokens for the summary
    #[serde(default = "default_max_tokens")]
    max_tokens: u32,
}

fn default_max_tokens() -> u32 {
    200
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConfirmDeleteInput {
    /// Name of the item to delete
    item: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SlowTaskInput {
    /// Number of steps (1-10)
    #[serde(default = "default_steps")]
    steps: u32,
}

fn default_steps() -> u32 {
    5
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=debug")
        .with_writer(std::io::stderr)
        .init();

    // Tool that requests LLM sampling from the client
    let summarize = ToolBuilder::new("summarize")
        .description("Summarize text by requesting LLM inference from the client")
        .read_only()
        .extractor_handler(
            (),
            |ctx: Context, Json(input): Json<SummarizeInput>| async move {
                let params = CreateMessageParams::new(
                    vec![SamplingMessage {
                        role: ContentRole::User,
                        content: SamplingContentOrArray::Single(SamplingContent::Text {
                            text: format!(
                                "Please summarize the following text in 1-2 sentences:\n\n{}",
                                input.text
                            ),
                            annotations: None,
                            meta: None,
                        }),
                        meta: None,
                    }],
                    input.max_tokens,
                )
                .system_prompt("You are a concise summarizer.");

                match ctx.sample(params).await {
                    Ok(result) => {
                        let summary = result.first_text().unwrap_or("(no text)");
                        Ok(CallToolResult::text(summary))
                    }
                    Err(e) => Ok(CallToolResult::error(format!("Sampling failed: {}", e))),
                }
            },
        )
        .build();

    // Tool that requests user confirmation via elicitation
    let confirm_delete = ToolBuilder::new("confirm_delete")
        .description("Delete an item after getting user confirmation via elicitation")
        .extractor_handler(
            (),
            |ctx: Context, Json(input): Json<ConfirmDeleteInput>| async move {
                let params = ElicitFormParams {
                    message: format!(
                        "Are you sure you want to delete '{}'? This cannot be undone.",
                        input.item
                    ),
                    requested_schema: ElicitFormSchema::new()
                        .boolean_field("confirmed", Some("Check to confirm deletion"), true)
                        .string_field("reason", Some("Why are you deleting this?"), false),
                    mode: None,
                    meta: None,
                };

                match ctx.elicit_form(params).await {
                    Ok(result) => match result.action {
                        ElicitAction::Accept => {
                            let confirmed = result
                                .content
                                .as_ref()
                                .and_then(|c| c.get("confirmed"))
                                .and_then(|v| match v {
                                    tower_mcp::protocol::ElicitFieldValue::Boolean(b) => Some(*b),
                                    _ => None,
                                })
                                .unwrap_or(false);

                            if confirmed {
                                let reason = result
                                    .content
                                    .as_ref()
                                    .and_then(|c| c.get("reason"))
                                    .and_then(|v| match v {
                                        tower_mcp::protocol::ElicitFieldValue::String(s) => {
                                            Some(s.as_str())
                                        }
                                        _ => None,
                                    })
                                    .unwrap_or("no reason given");
                                Ok(CallToolResult::text(format!(
                                    "Deleted '{}' (reason: {})",
                                    input.item, reason
                                )))
                            } else {
                                Ok(CallToolResult::text(format!(
                                    "Deletion of '{}' cancelled (not confirmed)",
                                    input.item
                                )))
                            }
                        }
                        other => Ok(CallToolResult::text(format!(
                            "Deletion of '{}' cancelled by user (action: {:?})",
                            input.item, other
                        ))),
                    },
                    Err(e) => Ok(CallToolResult::error(format!("Elicitation failed: {}", e))),
                }
            },
        )
        .build();

    // Tool that reports progress
    let slow_task = ToolBuilder::new("slow_task")
        .description("Simulate a task that reports progress over multiple steps")
        .read_only()
        .extractor_handler(
            (),
            |ctx: Context, Json(input): Json<SlowTaskInput>| async move {
                let steps = input.steps.min(10);
                for i in 0..steps {
                    ctx.report_progress(
                        i as f64,
                        Some(steps as f64),
                        Some(&format!("Processing step {}/{}", i + 1, steps)),
                    )
                    .await;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                ctx.report_progress(steps as f64, Some(steps as f64), Some("Complete"))
                    .await;
                Ok(CallToolResult::text(format!("Completed {} steps", steps)))
            },
        )
        .build();

    let router = McpRouter::new()
        .server_info("sampling-server", "1.0.0")
        .auto_instructions()
        .tool(summarize)
        .tool(confirm_delete)
        .tool(slow_task);

    let mut transport = StdioTransport::new(router);
    tracing::info!("Sampling server started");
    transport.run().await?;

    Ok(())
}
