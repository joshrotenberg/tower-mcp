//! MCP tool definitions for code generation.
//!
//! This module provides two sets of tools:
//!
//! 1. **Project-level tools** (original): `init_project`, `add_tool`, `generate`, etc.
//! 2. **Component builder tools** (new): `tool_new`, `tool_add_input`, `tool_build`, etc.
//!
//! The component builder tools support incremental construction with explicit
//! variant discovery, making them more suitable for AI agent workflows.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, NoParams, Tool, ToolBuilder,
    extract::{Json, State},
};

use crate::codegen::{generate_code, generate_tool_code};
use crate::state::{
    HandlerType, InputField, InputTypes, LayerConfig, LayerType, ProjectState, ToolAnnotations,
    ToolBuilderState, ToolDef, Transport,
};

/// Shared state for the codegen server.
pub struct CodegenState {
    /// Project-level state (for full project generation)
    pub project: RwLock<ProjectState>,
    /// In-progress tool builders (for component-level generation)
    pub building_tools: RwLock<HashMap<String, ToolBuilderState>>,
    /// Counter for generating unique IDs
    next_id: std::sync::atomic::AtomicU64,
}

impl CodegenState {
    pub fn new() -> Self {
        Self {
            project: RwLock::new(ProjectState::new()),
            building_tools: RwLock::new(HashMap::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Generate a unique ID for a new component.
    pub fn next_id(&self) -> String {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("tool_{}", id)
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
        // Project-level tools (original)
        build_init_project(state.clone()),
        build_add_tool(state.clone()),
        build_remove_tool(state.clone()),
        build_get_project(state.clone()),
        build_generate(state.clone()),
        build_validate(state.clone()),
        build_reset(state.clone()),
        // Discovery tools (teach the API)
        build_list_handler_types(),
        build_list_layer_types(),
        build_list_input_types(),
        // Component builder tools
        build_tool_new(state.clone()),
        build_tool_set_description(state.clone()),
        build_tool_add_input(state.clone()),
        build_tool_set_handler(state.clone()),
        build_tool_set_annotation(state.clone()),
        build_tool_add_layer(state.clone()),
        build_tool_get(state.clone()),
        build_tool_build(state.clone()),
        build_tool_add_to_project(state.clone()),
        build_tool_list(state.clone()),
        build_tool_discard(state.clone()),
    ]
}

fn build_init_project(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("init_project")
        .description("Initialize a new tower-mcp server project")
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>,
             Json(input): Json<InitProjectInput>| async move {
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
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>, Json(input): Json<AddToolInput>| async move {
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
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>,
             Json(input): Json<RemoveToolInput>| async move {
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
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>, Json(_): Json<NoParams>| async move {
                let project = state.project.read().await;
                CallToolResult::from_serialize(&*project)
            },
        )
        .build()
        .expect("valid tool")
}

fn build_generate(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("generate")
        .description("Generate the complete Rust code for the project")
        .read_only()
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>, Json(_): Json<NoParams>| async move {
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
            },
        )
        .build()
        .expect("valid tool")
}

fn build_validate(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("validate")
        .description("Validate the generated code compiles (runs cargo check)")
        .read_only()
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>, Json(_): Json<NoParams>| async move {
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
            },
        )
        .build()
        .expect("valid tool")
}

fn build_reset(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("reset")
        .description("Reset the project state to start over")
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>, Json(_): Json<NoParams>| async move {
                let mut project = state.project.write().await;
                project.reset();
                Ok(CallToolResult::text(
                    "Project state reset. Ready for new project.",
                ))
            },
        )
        .build()
        .expect("valid tool")
}

// =============================================================================
// Discovery Tools - Teach the API
// =============================================================================

fn build_list_handler_types() -> Tool {
    ToolBuilder::new("list_handler_types")
        .description("List all available handler types for tools. Use this to discover what handler types are available before calling tool_set_handler.")
        .read_only()
        .no_params_handler(|| async {
            let types: Vec<_> = HandlerType::all()
                .into_iter()
                .map(|t| {
                    let ht = match t {
                        "simple" => HandlerType::Simple,
                        "with_state" => HandlerType::WithState,
                        "with_context" => HandlerType::WithContext,
                        "with_state_and_context" => HandlerType::WithStateAndContext,
                        "raw" => HandlerType::Raw,
                        "no_params" => HandlerType::NoParams,
                        _ => HandlerType::Simple,
                    };
                    format!("- **{}**: {}", t, ht.description())
                })
                .collect();

            Ok(CallToolResult::text(format!(
                "# Available Handler Types\n\n{}\n\n**Usage**: `tool_set_handler(id, \"<type>\")`",
                types.join("\n")
            )))
        })
        .build()
        .expect("valid tool")
}

fn build_list_layer_types() -> Tool {
    ToolBuilder::new("list_layer_types")
        .description("List all available middleware layer types. Use this to discover what layers can be added to tools.")
        .read_only()
        .no_params_handler(|| async {
            let types: Vec<_> = [LayerType::Timeout, LayerType::RateLimit, LayerType::ConcurrencyLimit]
                .into_iter()
                .map(|lt| {
                    format!(
                        "- **{}**: {}\n  Config: `{}`",
                        lt,
                        lt.description(),
                        lt.config_schema()
                    )
                })
                .collect();

            Ok(CallToolResult::text(format!(
                "# Available Layer Types\n\n{}\n\n**Usage**: `tool_add_layer(id, \"<type>\", {{config}})`",
                types.join("\n\n")
            )))
        })
        .build()
        .expect("valid tool")
}

fn build_list_input_types() -> Tool {
    ToolBuilder::new("list_input_types")
        .description("List all available input field types. Use this to discover what types can be used for tool input fields.")
        .read_only()
        .no_params_handler(|| async {
            let types: Vec<_> = InputTypes::all()
                .into_iter()
                .map(|(t, desc)| format!("- **{}**: {}", t, desc))
                .collect();

            Ok(CallToolResult::text(format!(
                "# Available Input Types\n\n{}\n\n**Usage**: `tool_add_input(id, \"name\", \"<type>\", \"description\", required)`",
                types.join("\n")
            )))
        })
        .build()
        .expect("valid tool")
}

// =============================================================================
// Component Builder Tools
// =============================================================================

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ToolNewInput {
    /// Tool name (snake_case, e.g., "search_users")
    pub name: String,
}

fn build_tool_new(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("tool_new")
        .description("Start building a new tool. Returns an ID to use with other tool_* commands.")
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>, Json(input): Json<ToolNewInput>| async move {
                let id = state.next_id();
                let builder = ToolBuilderState::new(&input.name);

                let mut tools = state.building_tools.write().await;
                tools.insert(id.clone(), builder);

                Ok(CallToolResult::text(format!(
                    "Started building tool '{}'. ID: **{}**\n\nNext steps:\n- `tool_set_description({}, \"...\")`\n- `tool_add_input({}, ...)`\n- `tool_set_handler({}, \"...\")`\n- `tool_build({})` when done",
                    input.name, id, id, id, id, id
                )))
            },
        )
        .build()
        .expect("valid tool")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ToolSetDescriptionInput {
    /// Tool builder ID from tool_new
    pub id: String,
    /// Description of what the tool does
    pub description: String,
}

fn build_tool_set_description(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("tool_set_description")
        .description("Set the description for a tool being built")
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>,
             Json(input): Json<ToolSetDescriptionInput>| async move {
                let mut tools = state.building_tools.write().await;

                match tools.get_mut(&input.id) {
                    Some(builder) => {
                        builder.description = Some(input.description.clone());
                        Ok(CallToolResult::text(format!(
                            "Set description for tool '{}': \"{}\"",
                            builder.name, input.description
                        )))
                    }
                    None => Ok(CallToolResult::error(format!(
                        "Tool builder '{}' not found. Use tool_new first.",
                        input.id
                    ))),
                }
            },
        )
        .build()
        .expect("valid tool")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ToolAddInputInput {
    /// Tool builder ID from tool_new
    pub id: String,
    /// Field name (snake_case)
    pub name: String,
    /// Field type (use list_input_types to see options)
    #[serde(rename = "type")]
    pub field_type: String,
    /// Field description
    pub description: String,
    /// Whether the field is required (default: true)
    #[serde(default = "default_true")]
    pub required: bool,
}

fn build_tool_add_input(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("tool_add_input")
        .description("Add an input field to a tool being built. Use list_input_types to see available types.")
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>,
             Json(input): Json<ToolAddInputInput>| async move {
                let mut tools = state.building_tools.write().await;

                match tools.get_mut(&input.id) {
                    Some(builder) => {
                        let field = InputField {
                            name: input.name.clone(),
                            field_type: input.field_type.clone(),
                            description: input.description.clone(),
                            required: input.required,
                        };
                        builder.input_fields.push(field);

                        let req = if input.required { "required" } else { "optional" };
                        Ok(CallToolResult::text(format!(
                            "Added {} input '{}' ({}) to tool '{}'. Total inputs: {}",
                            req,
                            input.name,
                            input.field_type,
                            builder.name,
                            builder.input_fields.len()
                        )))
                    }
                    None => Ok(CallToolResult::error(format!(
                        "Tool builder '{}' not found. Use tool_new first.",
                        input.id
                    ))),
                }
            },
        )
        .build()
        .expect("valid tool")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ToolSetHandlerInput {
    /// Tool builder ID from tool_new
    pub id: String,
    /// Handler type (use list_handler_types to see options)
    pub handler_type: String,
}

fn build_tool_set_handler(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("tool_set_handler")
        .description("Set the handler type for a tool. Use list_handler_types to see available types.")
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>,
             Json(input): Json<ToolSetHandlerInput>| async move {
                let handler_type = match input.handler_type.to_lowercase().as_str() {
                    "simple" => HandlerType::Simple,
                    "with_state" => HandlerType::WithState,
                    "with_context" => HandlerType::WithContext,
                    "with_state_and_context" => HandlerType::WithStateAndContext,
                    "raw" => HandlerType::Raw,
                    "no_params" => HandlerType::NoParams,
                    _ => {
                        return Ok(CallToolResult::error(format!(
                            "Unknown handler type '{}'. Use list_handler_types to see available types.",
                            input.handler_type
                        )));
                    }
                };

                let mut tools = state.building_tools.write().await;

                match tools.get_mut(&input.id) {
                    Some(builder) => {
                        builder.handler_type = handler_type.clone();
                        Ok(CallToolResult::text(format!(
                            "Set handler type for tool '{}' to '{}'\n\n{}",
                            builder.name,
                            input.handler_type,
                            handler_type.description()
                        )))
                    }
                    None => Ok(CallToolResult::error(format!(
                        "Tool builder '{}' not found. Use tool_new first.",
                        input.id
                    ))),
                }
            },
        )
        .build()
        .expect("valid tool")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ToolSetAnnotationInput {
    /// Tool builder ID from tool_new
    pub id: String,
    /// Annotation name: "read_only", "idempotent", or "long_running"
    pub annotation: String,
    /// Annotation value
    pub value: bool,
}

fn build_tool_set_annotation(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("tool_set_annotation")
        .description("Set an annotation on a tool. Annotations are hints about tool behavior.")
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>,
             Json(input): Json<ToolSetAnnotationInput>| async move {
                let mut tools = state.building_tools.write().await;

                match tools.get_mut(&input.id) {
                    Some(builder) => {
                        match input.annotation.to_lowercase().as_str() {
                            "read_only" => builder.annotations.read_only = input.value,
                            "idempotent" => builder.annotations.idempotent = input.value,
                            "long_running" => builder.annotations.long_running = input.value,
                            _ => {
                                return Ok(CallToolResult::error(format!(
                                    "Unknown annotation '{}'. Available: read_only, idempotent, long_running",
                                    input.annotation
                                )));
                            }
                        }
                        Ok(CallToolResult::text(format!(
                            "Set {} = {} for tool '{}'",
                            input.annotation, input.value, builder.name
                        )))
                    }
                    None => Ok(CallToolResult::error(format!(
                        "Tool builder '{}' not found. Use tool_new first.",
                        input.id
                    ))),
                }
            },
        )
        .build()
        .expect("valid tool")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ToolAddLayerInput {
    /// Tool builder ID from tool_new
    pub id: String,
    /// Layer type (use list_layer_types to see options)
    pub layer_type: String,
    /// Layer configuration (depends on layer type)
    #[serde(default)]
    pub config: serde_json::Value,
}

fn build_tool_add_layer(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("tool_add_layer")
        .description("Add a middleware layer to a tool. Use list_layer_types to see available layers and their config.")
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>,
             Json(input): Json<ToolAddLayerInput>| async move {
                let layer_type = match input.layer_type.to_lowercase().as_str() {
                    "timeout" => LayerType::Timeout,
                    "rate_limit" => LayerType::RateLimit,
                    "concurrency_limit" => LayerType::ConcurrencyLimit,
                    _ => {
                        return Ok(CallToolResult::error(format!(
                            "Unknown layer type '{}'. Use list_layer_types to see available types.",
                            input.layer_type
                        )));
                    }
                };

                let mut tools = state.building_tools.write().await;

                match tools.get_mut(&input.id) {
                    Some(builder) => {
                        let layer = LayerConfig {
                            layer_type: layer_type.clone(),
                            config: input.config.clone(),
                        };
                        builder.layers.push(layer);

                        Ok(CallToolResult::text(format!(
                            "Added {} layer to tool '{}'. Config: {}",
                            layer_type,
                            builder.name,
                            serde_json::to_string_pretty(&input.config).unwrap_or_default()
                        )))
                    }
                    None => Ok(CallToolResult::error(format!(
                        "Tool builder '{}' not found. Use tool_new first.",
                        input.id
                    ))),
                }
            },
        )
        .build()
        .expect("valid tool")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ToolIdInput {
    /// Tool builder ID from tool_new
    pub id: String,
}

fn build_tool_get(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("tool_get")
        .description("Get the current state of a tool being built")
        .read_only()
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>, Json(input): Json<ToolIdInput>| async move {
                let tools = state.building_tools.read().await;

                match tools.get(&input.id) {
                    Some(builder) => CallToolResult::from_serialize(builder),
                    None => Ok(CallToolResult::error(format!(
                        "Tool builder '{}' not found.",
                        input.id
                    ))),
                }
            },
        )
        .build()
        .expect("valid tool")
}

fn build_tool_build(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("tool_build")
        .description("Generate Rust code for the tool. Returns the code but keeps the builder for further modifications.")
        .read_only()
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>, Json(input): Json<ToolIdInput>| async move {
                let tools = state.building_tools.read().await;

                match tools.get(&input.id) {
                    Some(builder) => {
                        match generate_tool_code(builder) {
                            Ok(code) => Ok(CallToolResult::text(format!(
                                "# Generated Code for tool '{}'\n\n## Input Struct\n\n```rust\n{}\n```\n\n## Tool Builder\n\n```rust\n{}\n```\n\n---\n\n**Note**: You'll need to implement the handler body. The `todo!()` placeholder shows where your logic goes.",
                                builder.name,
                                code.input_struct,
                                code.tool_builder
                            ))),
                            Err(e) => Ok(CallToolResult::error(e)),
                        }
                    }
                    None => Ok(CallToolResult::error(format!(
                        "Tool builder '{}' not found.",
                        input.id
                    ))),
                }
            },
        )
        .build()
        .expect("valid tool")
}

fn build_tool_add_to_project(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("tool_add_to_project")
        .description("Add the built tool to the project for full project generation")
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>, Json(input): Json<ToolIdInput>| async move {
                let mut tools = state.building_tools.write().await;
                let mut project = state.project.write().await;

                match tools.remove(&input.id) {
                    Some(builder) => {
                        let name = builder.name.clone();

                        if !project.initialized {
                            return Ok(CallToolResult::error(
                                "Project not initialized. Call init_project first, or use tool_build to get the code directly.",
                            ));
                        }

                        if project.tools.iter().any(|t| t.name == name) {
                            return Ok(CallToolResult::error(format!(
                                "Tool '{}' already exists in project",
                                name
                            )));
                        }

                        project.tools.push(builder.into_tool_def());

                        Ok(CallToolResult::text(format!(
                            "Added tool '{}' to project. Project now has {} tool(s). Use generate to create the full project code.",
                            name,
                            project.tools.len()
                        )))
                    }
                    None => Ok(CallToolResult::error(format!(
                        "Tool builder '{}' not found.",
                        input.id
                    ))),
                }
            },
        )
        .build()
        .expect("valid tool")
}

fn build_tool_list(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("tool_list")
        .description("List all tools currently being built")
        .read_only()
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>, Json(_): Json<NoParams>| async move {
                let tools = state.building_tools.read().await;

                if tools.is_empty() {
                    return Ok(CallToolResult::text(
                        "No tools currently being built. Use tool_new to start building a tool.",
                    ));
                }

                let list: Vec<_> = tools
                    .iter()
                    .map(|(id, builder)| {
                        format!(
                            "- **{}**: {} ({} inputs, handler: {:?})",
                            id,
                            builder.name,
                            builder.input_fields.len(),
                            builder.handler_type
                        )
                    })
                    .collect();

                Ok(CallToolResult::text(format!(
                    "# Tools Being Built\n\n{}",
                    list.join("\n")
                )))
            },
        )
        .build()
        .expect("valid tool")
}

fn build_tool_discard(state: Arc<CodegenState>) -> Tool {
    ToolBuilder::new("tool_discard")
        .description("Discard a tool being built without adding it to the project")
        .extractor_handler(
            state,
            |State(state): State<Arc<CodegenState>>, Json(input): Json<ToolIdInput>| async move {
                let mut tools = state.building_tools.write().await;

                match tools.remove(&input.id) {
                    Some(builder) => Ok(CallToolResult::text(format!(
                        "Discarded tool '{}' ({})",
                        builder.name, input.id
                    ))),
                    None => Ok(CallToolResult::error(format!(
                        "Tool builder '{}' not found.",
                        input.id
                    ))),
                }
            },
        )
        .build()
        .expect("valid tool")
}
