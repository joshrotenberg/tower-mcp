//! MCP tool definitions for code generation.

use std::sync::Arc;
use tokio::sync::RwLock;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{CallToolResult, Tool, ToolBuilder};

use crate::codegen::generate_code;
use crate::state::{HandlerType, InputField, ProjectState, ToolAnnotations, ToolDef, Transport};

/// Shared state for the codegen server.
pub struct CodegenState {
    pub project: RwLock<ProjectState>,
}

impl CodegenState {
    pub fn new() -> Self {
        Self {
            project: RwLock::new(ProjectState::new()),
        }
    }
}

// =============================================================================
// Tool Input Types
// =============================================================================

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InitProjectInput {
    /// Project name (will be used as crate name, should be snake_case)
    pub name: String,

    /// Project description
    #[serde(default)]
    pub description: Option<String>,

    /// Transports to enable (stdio, http, websocket). Defaults to stdio.
    #[serde(default)]
    pub transports: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddToolInput {
    /// Tool name (snake_case)
    pub name: String,

    /// Tool description
    pub description: String,

    /// Input fields for the tool
    #[serde(default)]
    pub input_fields: Vec<InputFieldInput>,

    /// Handler type: simple, with_state, with_context, with_state_and_context, raw, no_params
    #[serde(default = "default_handler_type")]
    pub handler_type: String,

    /// Whether the tool is read-only
    #[serde(default)]
    pub read_only: bool,

    /// Whether the tool is idempotent
    #[serde(default)]
    pub idempotent: bool,
}

fn default_handler_type() -> String {
    "simple".to_string()
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InputFieldInput {
    /// Field name
    pub name: String,

    /// Field type (String, i64, f64, bool, Option<T>, Vec<T>)
    #[serde(rename = "type")]
    pub field_type: String,

    /// Field description
    pub description: String,

    /// Whether the field is required (default: true)
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_true() -> bool {
    true
}

// =============================================================================
// Tool Builders
// =============================================================================

/// Build all codegen tools.
pub fn build_tools(state: Arc<CodegenState>) -> Vec<Tool> {
    vec![
        build_init_project(state.clone()),
        build_add_tool(state.clone()),
        build_remove_tool(state.clone()),
        build_get_project(state.clone()),
        build_generate(state.clone()),
        build_validate(state.clone()),
        build_reset(state.clone()),
    ]
}

fn build_init_project(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("init_project")
        .description("Initialize a new tower-mcp server project")
        .handler_with_state(
            state,
            |state: Arc<CodegenState>, input: InitProjectInput| async move {
                let mut project = state.project.write().await;

                if project.initialized {
                    return Ok(CallToolResult::error(
                        "Project already initialized. Call reset first to start over.",
                    ));
                }

                // Parse transports
                let transports: Vec<Transport> = if input.transports.is_empty() {
                    vec![Transport::Stdio]
                } else {
                    input
                        .transports
                        .iter()
                        .filter_map(|t| match t.to_lowercase().as_str() {
                            "stdio" => Some(Transport::Stdio),
                            "http" => Some(Transport::Http),
                            "websocket" | "ws" => Some(Transport::WebSocket),
                            _ => None,
                        })
                        .collect()
                };

                project.initialized = true;
                project.name = Some(input.name.clone());
                project.description = input.description;
                project.transports = transports.clone();

                let transport_names: Vec<_> = transports.iter().map(|t| t.to_string()).collect();
                Ok(CallToolResult::text(format!(
                    "Project '{}' initialized with transports: {}",
                    input.name,
                    transport_names.join(", ")
                )))
            },
        )
        .build()
        .expect("valid tool")
}

fn build_add_tool(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("add_tool")
        .description("Add a tool to the project")
        .handler_with_state(
            state,
            |state: Arc<CodegenState>, input: AddToolInput| async move {
                let mut project = state.project.write().await;

                if !project.initialized {
                    return Ok(CallToolResult::error(
                        "Project not initialized. Call init_project first.",
                    ));
                }

                // Check for duplicate
                if project.tools.iter().any(|t| t.name == input.name) {
                    return Ok(CallToolResult::error(format!(
                        "Tool '{}' already exists",
                        input.name
                    )));
                }

                // Parse handler type
                let handler_type = match input.handler_type.to_lowercase().as_str() {
                    "simple" => HandlerType::Simple,
                    "with_state" => HandlerType::WithState,
                    "with_context" => HandlerType::WithContext,
                    "with_state_and_context" => HandlerType::WithStateAndContext,
                    "raw" => HandlerType::Raw,
                    "no_params" => HandlerType::NoParams,
                    _ => HandlerType::Simple,
                };

                // Convert input fields
                let input_fields: Vec<InputField> = input
                    .input_fields
                    .into_iter()
                    .map(|f| InputField {
                        name: f.name,
                        field_type: f.field_type,
                        description: f.description,
                        required: f.required,
                    })
                    .collect();

                let tool = ToolDef {
                    name: input.name.clone(),
                    description: input.description,
                    input_fields,
                    handler_type,
                    annotations: ToolAnnotations {
                        read_only: input.read_only,
                        idempotent: input.idempotent,
                        long_running: false,
                    },
                };

                project.tools.push(tool);

                Ok(CallToolResult::text(format!(
                    "Added tool '{}'. Project now has {} tool(s).",
                    input.name,
                    project.tools.len()
                )))
            },
        )
        .build()
        .expect("valid tool")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveToolInput {
    /// Name of the tool to remove
    pub name: String,
}

fn build_remove_tool(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("remove_tool")
        .description("Remove a tool from the project")
        .handler_with_state(
            state,
            |state: Arc<CodegenState>, input: RemoveToolInput| async move {
                let mut project = state.project.write().await;

                if !project.initialized {
                    return Ok(CallToolResult::error(
                        "Project not initialized. Call init_project first.",
                    ));
                }

                let initial_len = project.tools.len();
                project.tools.retain(|t| t.name != input.name);

                if project.tools.len() == initial_len {
                    Ok(CallToolResult::error(format!(
                        "Tool '{}' not found",
                        input.name
                    )))
                } else {
                    Ok(CallToolResult::text(format!(
                        "Removed tool '{}'. Project now has {} tool(s).",
                        input.name,
                        project.tools.len()
                    )))
                }
            },
        )
        .build()
        .expect("valid tool")
}

fn build_get_project(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("get_project")
        .description("Get the current project state as JSON")
        .read_only()
        .handler_with_state_no_params(state, |state: Arc<CodegenState>| async move {
            let project = state.project.read().await;
            CallToolResult::from_serialize(&*project)
        })
        .expect("valid tool")
}

fn build_generate(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("generate")
        .description("Generate the complete Rust code for the project")
        .read_only()
        .handler_with_state_no_params(state, |state: Arc<CodegenState>| async move {
            let project = state.project.read().await;

            match generate_code(&project) {
                Ok(code) => {
                    let output = format!(
                        "# Generated Code\n\n## Cargo.toml\n\n```toml\n{}\n```\n\n## src/main.rs\n\n```rust\n{}\n```\n\n## README.md\n\n{}\n\n---\n\n**What's next?** You can stop here and implement the handlers manually, or keep using codegen-mcp to add more tools. Read `project://README.md` for details.",
                        code.cargo_toml, code.main_rs, code.readme_md
                    );
                    Ok(CallToolResult::text(output))
                }
                Err(e) => Ok(CallToolResult::error(e)),
            }
        })
        .expect("valid tool")
}

fn build_validate(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("validate")
        .description("Validate the generated code compiles (runs cargo check)")
        .read_only()
        .handler_with_state_no_params(state, |state: Arc<CodegenState>| async move {
            let project = state.project.read().await;

            // Generate the code
            let code = match generate_code(&project) {
                Ok(code) => code,
                Err(e) => return Ok(CallToolResult::error(e)),
            };

            // Create temp directory
            let temp_dir = match tempfile::tempdir() {
                Ok(dir) => dir,
                Err(e) => {
                    return Ok(CallToolResult::error(format!(
                        "Failed to create temp dir: {}",
                        e
                    )));
                }
            };

            let project_dir = temp_dir.path();
            let src_dir = project_dir.join("src");

            // Create src directory
            if let Err(e) = std::fs::create_dir_all(&src_dir) {
                return Ok(CallToolResult::error(format!(
                    "Failed to create src dir: {}",
                    e
                )));
            }

            // Write Cargo.toml
            if let Err(e) = std::fs::write(project_dir.join("Cargo.toml"), &code.cargo_toml) {
                return Ok(CallToolResult::error(format!(
                    "Failed to write Cargo.toml: {}",
                    e
                )));
            }

            // Write main.rs
            if let Err(e) = std::fs::write(src_dir.join("main.rs"), &code.main_rs) {
                return Ok(CallToolResult::error(format!(
                    "Failed to write main.rs: {}",
                    e
                )));
            }

            // Run cargo check
            let output = match std::process::Command::new("cargo")
                .arg("check")
                .current_dir(project_dir)
                .output()
            {
                Ok(output) => output,
                Err(e) => {
                    return Ok(CallToolResult::error(format!(
                        "Failed to run cargo check: {}",
                        e
                    )));
                }
            };

            if output.status.success() {
                Ok(CallToolResult::text(
                    "Validation successful! Generated code compiles.",
                ))
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Ok(CallToolResult::error(format!(
                    "Compilation failed:\n{}",
                    stderr
                )))
            }
        })
        .expect("valid tool")
}

fn build_reset(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("reset")
        .description("Reset the project state to start over")
        .handler_with_state_no_params(state, |state: Arc<CodegenState>| async move {
            let mut project = state.project.write().await;
            project.reset();
            Ok(CallToolResult::text(
                "Project state reset. Ready for new project.",
            ))
        })
        .expect("valid tool")
}
