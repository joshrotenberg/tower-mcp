//! MCP tool definitions for markdown linting.
//!
//! This module defines the MCP tools exposed by the markdownlint server.
//! Each tool is built using tower-mcp's [`ToolBuilder`] API.
//!
//! # Tool Design Patterns
//!
//! This module demonstrates several tower-mcp patterns:
//!
//! ## Shared State
//!
//! Tools share a common [`LintState`] via `Arc`:
//!
//! ```rust,ignore
//! let state = Arc::new(LintState::new()?);
//!
//! // Each tool gets a clone of the Arc
//! let tool = ToolBuilder::new("lint_content")
//!     .handler_with_state(state.clone(), |state, input| async move {
//!         // Use state here
//!     })
//!     .build()?;
//! ```
//!
//! ## Input Types with JsonSchema
//!
//! Tool inputs derive `JsonSchema` for automatic schema generation:
//!
//! ```rust,ignore
//! #[derive(Debug, Deserialize, JsonSchema)]
//! pub struct LintContentInput {
//!     /// Markdown content to lint
//!     pub content: String,
//!
//!     /// Optional filename for context
//!     #[serde(default = "default_filename")]
//!     pub filename: String,
//! }
//! ```
//!
//! ## Error Handling
//!
//! Tools return `CallToolResult::error()` for user-facing errors:
//!
//! ```rust,ignore
//! match state.lint(&content, &path) {
//!     Ok(violations) => CallToolResult::from_serialize(&result),
//!     Err(e) => Ok(CallToolResult::error(format!("Lint failed: {}", e))),
//! }
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::protocol::{Content, GetPromptResult, PromptMessage, PromptRole};
use tower_mcp::{CallToolResult, Prompt, PromptBuilder, Tool, ToolBuilder};

use crate::engine::{LintState, RulesListResult};

// =============================================================================
// Tool Input Types
// =============================================================================

/// Input for the `lint_content` tool.
///
/// Lints markdown content provided directly as a string.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LintContentInput {
    /// Markdown content to lint.
    ///
    /// Can be any valid UTF-8 string containing markdown.
    pub content: String,

    /// Optional filename for context in error messages.
    ///
    /// Defaults to "input.md" if not provided. This doesn't need to be
    /// a real file path - it's used for display purposes in violations.
    #[serde(default = "default_filename")]
    pub filename: String,
}

/// Input for the `lint_file` tool.
///
/// Lints a markdown file from the local filesystem.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LintFileInput {
    /// Absolute or relative path to the markdown file.
    ///
    /// The file must be readable by the server process.
    pub path: String,
}

/// Input for the `lint_url` tool.
///
/// Fetches markdown content from a URL and lints it.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LintUrlInput {
    /// URL to fetch markdown content from.
    ///
    /// Must be a valid HTTP or HTTPS URL. The response body is
    /// treated as markdown content.
    pub url: String,
}

/// Input for the `explain_rule` tool.
///
/// Gets detailed information about a specific lint rule.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExplainRuleInput {
    /// Rule identifier to look up.
    ///
    /// Examples: "MD001", "MD012", "MDBOOK003"
    pub rule_id: String,
}

/// Input for the `fix_content` tool.
///
/// Applies automatic fixes to markdown content.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FixContentInput {
    /// Markdown content to fix.
    pub content: String,

    /// Optional filename for context.
    ///
    /// Defaults to "input.md" if not provided.
    #[serde(default = "default_filename")]
    pub filename: String,
}

/// Default filename for content without an explicit path.
fn default_filename() -> String {
    "input.md".to_string()
}

// =============================================================================
// Tool Builder Functions
// =============================================================================

/// Build all linting tools.
///
/// Creates the shared [`LintState`] and returns all tool definitions.
/// This is the main entry point called from `main.rs`.
///
/// # Panics
///
/// Panics if the lint engine fails to initialize. This is intentional
/// as the server cannot function without the linting engine.
pub fn build_tools() -> Vec<Tool> {
    let state = Arc::new(LintState::new().expect("Failed to create lint engine"));

    vec![
        build_lint_content_tool(state.clone()),
        build_lint_file_tool(state.clone()),
        build_lint_url_tool(state.clone()),
        build_list_rules_tool(state.clone()),
        build_explain_rule_tool(state.clone()),
        build_fix_content_tool(state.clone()),
    ]
}

/// Build the `lint_content` tool.
///
/// This tool lints markdown content provided directly as a string.
/// It's useful for checking content that isn't saved to a file yet.
fn build_lint_content_tool(state: Arc<LintState>) -> Tool {
    ToolBuilder::new("lint_content")
        .description("Lint markdown content and return violations")
        .handler_with_state(
            state,
            |state: Arc<LintState>, input: LintContentInput| async move {
                match state.lint_to_result(&input.content, &input.filename) {
                    Ok(result) => CallToolResult::from_serialize(&result),
                    Err(e) => Ok(CallToolResult::error(e)),
                }
            },
        )
        .build()
        .expect("valid tool")
}

/// Build the `lint_file` tool.
///
/// This tool reads a file from disk and lints its contents.
/// Useful for checking files in a project.
fn build_lint_file_tool(state: Arc<LintState>) -> Tool {
    ToolBuilder::new("lint_file")
        .description("Lint a local markdown file")
        .handler_with_state(
            state,
            |state: Arc<LintState>, input: LintFileInput| async move {
                // Read the file asynchronously
                let content = match tokio::fs::read_to_string(&input.path).await {
                    Ok(c) => c,
                    Err(e) => {
                        return Ok(CallToolResult::error(format!("Failed to read file: {}", e)));
                    }
                };

                match state.lint_to_result(&content, &input.path) {
                    Ok(result) => CallToolResult::from_serialize(&result),
                    Err(e) => Ok(CallToolResult::error(e)),
                }
            },
        )
        .build()
        .expect("valid tool")
}

/// Build the `lint_url` tool.
///
/// This tool fetches content from a URL and lints it.
/// Useful for checking remote documentation or README files.
fn build_lint_url_tool(state: Arc<LintState>) -> Tool {
    ToolBuilder::new("lint_url")
        .description("Fetch markdown from a URL and lint it")
        .handler_with_state(
            state,
            |state: Arc<LintState>, input: LintUrlInput| async move {
                // Fetch the URL
                let content = match reqwest::get(&input.url).await {
                    Ok(resp) => match resp.text().await {
                        Ok(text) => text,
                        Err(e) => {
                            return Ok(CallToolResult::error(format!(
                                "Failed to read response: {}",
                                e
                            )));
                        }
                    },
                    Err(e) => {
                        return Ok(CallToolResult::error(format!("Failed to fetch URL: {}", e)));
                    }
                };

                match state.lint_to_result(&content, &input.url) {
                    Ok(result) => CallToolResult::from_serialize(&result),
                    Err(e) => Ok(CallToolResult::error(e)),
                }
            },
        )
        .build()
        .expect("valid tool")
}

/// Build the `list_rules` tool.
///
/// This tool returns information about all available lint rules.
/// Useful for discovering what checks are available.
///
/// Note: Uses `handler_with_state_no_params` since no input is needed.
fn build_list_rules_tool(state: Arc<LintState>) -> Tool {
    ToolBuilder::new("list_rules")
        .description("List all available lint rules with their descriptions")
        .handler_with_state_no_params(state, |state: Arc<LintState>| async move {
            let rules = state.rules();
            let result = RulesListResult {
                total: rules.len(),
                rules,
            };
            CallToolResult::from_serialize(&result)
        })
        .expect("valid tool")
}

/// Build the `explain_rule` tool.
///
/// This tool returns detailed information about a specific rule.
/// Useful for understanding what a rule checks and whether it can auto-fix.
fn build_explain_rule_tool(state: Arc<LintState>) -> Tool {
    ToolBuilder::new("explain_rule")
        .description("Get detailed explanation of a specific lint rule")
        .handler_with_state(
            state,
            |state: Arc<LintState>, input: ExplainRuleInput| async move {
                match state.get_rule(&input.rule_id) {
                    Some(rule) => CallToolResult::from_serialize(&rule),
                    None => Ok(CallToolResult::error(format!(
                        "Rule '{}' not found",
                        input.rule_id
                    ))),
                }
            },
        )
        .build()
        .expect("valid tool")
}

/// Build the `fix_content` tool.
///
/// This tool applies automatic fixes to markdown content where possible.
/// Returns both the fixed content and any remaining violations.
fn build_fix_content_tool(state: Arc<LintState>) -> Tool {
    ToolBuilder::new("fix_content")
        .description("Apply automatic fixes to markdown content where possible")
        .handler_with_state(
            state,
            |state: Arc<LintState>, input: FixContentInput| async move {
                match state.fix_content(&input.content, &input.filename) {
                    Ok(result) => CallToolResult::from_serialize(&result),
                    Err(e) => Ok(CallToolResult::error(e)),
                }
            },
        )
        .build()
        .expect("valid tool")
}

// =============================================================================
// Prompts
// =============================================================================

/// Build the `fix_suggestions` prompt.
///
/// This prompt helps users understand how to fix lint violations.
/// It takes JSON output from the linting tools and generates
/// human-readable fix suggestions.
///
/// # Prompt Design
///
/// The prompt instructs the LLM to:
/// 1. Explain what each violated rule checks for
/// 2. Show the specific fix needed
/// 3. Mention auto-fix availability where applicable
pub fn build_fix_suggestions_prompt() -> Prompt {
    PromptBuilder::new("fix_suggestions")
        .description("Generate suggestions for fixing lint violations")
        .required_arg(
            "violations_json",
            "JSON output from lint_content or lint_file",
        )
        .handler(|args: HashMap<String, String>| async move {
            let violations_json = args.get("violations_json").cloned().unwrap_or_default();

            let prompt_text = format!(
                "Analyze the following markdown lint violations and provide specific \
                 suggestions for fixing each one.\n\n\
                 For each violation:\n\
                 1. Explain what the rule checks for\n\
                 2. Show the specific fix needed\n\
                 3. If the violation has an automatic fix available (has_fix: true), \
                    mention that the fix_content tool can be used\n\n\
                 Lint results:\n```json\n{}\n```",
                violations_json
            );

            Ok(GetPromptResult {
                description: Some("Suggestions for fixing markdown lint violations".to_string()),
                messages: vec![PromptMessage {
                    role: PromptRole::User,
                    content: Content::Text {
                        text: prompt_text,
                        annotations: None,
                    },
                }],
            })
        })
}
