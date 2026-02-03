//! Project state management for code generation.
//!
//! This module defines the data structures that accumulate project
//! configuration as the agent calls tools like `init_project`, `add_tool`, etc.
//!
//! ## Component Builder Pattern
//!
//! In addition to project-level state, this module supports incremental
//! component building where tools/resources/prompts are constructed step-by-step:
//!
//! ```text
//! tool_new("search") -> id
//! tool_set_description(id, "...")
//! tool_add_input(id, "query", "String", ...)
//! tool_set_handler(id, "with_state")
//! tool_build(id) -> Rust code
//! ```

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The accumulated project state.
///
/// This is built up incrementally through tool calls and used
/// to generate the final Rust code.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProjectState {
    /// Whether the project has been initialized
    pub initialized: bool,

    /// Project name (crate name)
    pub name: Option<String>,

    /// Project description
    pub description: Option<String>,

    /// Enabled transports
    pub transports: Vec<Transport>,

    /// Tool definitions
    pub tools: Vec<ToolDef>,

    /// Resource definitions
    pub resources: Vec<ResourceDef>,

    /// Prompt definitions
    pub prompts: Vec<PromptDef>,

    /// State fields (for shared state)
    pub state_fields: Vec<StateField>,
}

/// Supported transport types.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Stdio,
    Http,
    WebSocket,
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Transport::Stdio => write!(f, "stdio"),
            Transport::Http => write!(f, "http"),
            Transport::WebSocket => write!(f, "websocket"),
        }
    }
}

/// A tool definition.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolDef {
    /// Tool name (snake_case)
    pub name: String,

    /// Tool description
    pub description: String,

    /// Input fields for the tool
    pub input_fields: Vec<InputField>,

    /// Handler type
    pub handler_type: HandlerType,

    /// Tool annotations
    #[serde(default)]
    pub annotations: ToolAnnotations,
}

/// Input field definition.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InputField {
    /// Field name
    pub name: String,

    /// Field type (String, i64, f64, bool, Option<T>, Vec<T>)
    #[serde(rename = "type")]
    pub field_type: String,

    /// Field description (becomes doc comment)
    pub description: String,

    /// Whether the field is required
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_true() -> bool {
    true
}

/// Handler type variants.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum HandlerType {
    /// Simple handler: `handler(|input: T| async move { ... })`
    #[default]
    Simple,

    /// Handler with state: `handler_with_state(|state: Arc<S>, input: T| async move { ... })`
    WithState,

    /// Handler with context: `handler_with_context(|ctx: RequestContext, input: T| async move { ... })`
    WithContext,

    /// Handler with state and context
    WithStateAndContext,

    /// Raw handler (Value input): `raw_handler(|args: Value| async move { ... })`
    Raw,

    /// No params handler: `handler_no_params(|| async move { ... })`
    NoParams,
}

/// Tool annotations for hints to the model.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ToolAnnotations {
    /// Tool is read-only (doesn't modify state)
    #[serde(default)]
    pub read_only: bool,

    /// Tool is idempotent (safe to retry)
    #[serde(default)]
    pub idempotent: bool,

    /// Tool may take a long time
    #[serde(default)]
    pub long_running: bool,
}

/// A resource definition.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResourceDef {
    /// Resource URI
    pub uri: String,

    /// Resource name
    pub name: String,

    /// Resource description
    pub description: String,

    /// MIME type
    #[serde(default = "default_mime_type")]
    pub mime_type: String,

    /// Whether this is a template
    #[serde(default)]
    pub is_template: bool,
}

fn default_mime_type() -> String {
    "text/plain".to_string()
}

/// A prompt definition.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PromptDef {
    /// Prompt name
    pub name: String,

    /// Prompt description
    pub description: String,

    /// Prompt arguments
    pub arguments: Vec<PromptArg>,
}

/// A prompt argument.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PromptArg {
    /// Argument name
    pub name: String,

    /// Argument description
    pub description: String,

    /// Whether the argument is required
    #[serde(default = "default_true")]
    pub required: bool,
}

/// A state field for shared state.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StateField {
    /// Field name
    pub name: String,

    /// Field type
    #[serde(rename = "type")]
    pub field_type: String,

    /// Field description
    pub description: Option<String>,
}

impl ProjectState {
    /// Create a new empty project state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if the project has any tools.
    #[allow(dead_code)]
    pub fn has_tools(&self) -> bool {
        !self.tools.is_empty()
    }

    /// Check if the project uses shared state.
    pub fn has_state(&self) -> bool {
        !self.state_fields.is_empty()
    }

    /// Check if any tool uses state.
    pub fn any_tool_uses_state(&self) -> bool {
        self.tools.iter().any(|t| {
            matches!(
                t.handler_type,
                HandlerType::WithState | HandlerType::WithStateAndContext
            )
        })
    }

    /// Check if any tool uses context.
    pub fn any_tool_uses_context(&self) -> bool {
        self.tools.iter().any(|t| {
            matches!(
                t.handler_type,
                HandlerType::WithContext | HandlerType::WithStateAndContext
            )
        })
    }

    /// Reset the project state.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

// =============================================================================
// Component Builder State
// =============================================================================

/// In-progress tool being built incrementally.
///
/// Created by `tool_new`, configured by `tool_set_*` and `tool_add_*`,
/// finalized by `tool_build`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ToolBuilderState {
    /// Tool name (snake_case)
    pub name: String,

    /// Tool description
    pub description: Option<String>,

    /// Input fields
    pub input_fields: Vec<InputField>,

    /// Handler type
    #[serde(default)]
    pub handler_type: HandlerType,

    /// Tool annotations
    #[serde(default)]
    pub annotations: ToolAnnotations,

    /// Middleware layers to apply
    #[serde(default)]
    pub layers: Vec<LayerConfig>,
}

impl ToolBuilderState {
    /// Create a new tool builder with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }

    /// Convert to a ToolDef (for adding to project).
    pub fn into_tool_def(self) -> ToolDef {
        ToolDef {
            name: self.name,
            description: self.description.unwrap_or_default(),
            input_fields: self.input_fields,
            handler_type: self.handler_type,
            annotations: self.annotations,
        }
    }
}

/// Configuration for a middleware layer.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LayerConfig {
    /// Type of layer
    pub layer_type: LayerType,

    /// Layer-specific configuration
    #[serde(default)]
    pub config: Value,
}

/// Available middleware layer types.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LayerType {
    /// Timeout layer - fails if handler takes too long
    Timeout,
    /// Rate limit layer - limits requests per time window
    RateLimit,
    /// Concurrency limit layer - limits concurrent executions
    ConcurrencyLimit,
}

impl LayerType {
    /// Get all available layer types.
    #[allow(dead_code)]
    pub fn all() -> Vec<&'static str> {
        vec!["timeout", "rate_limit", "concurrency_limit"]
    }

    /// Get description for a layer type.
    pub fn description(&self) -> &'static str {
        match self {
            LayerType::Timeout => {
                "Fails the tool call if it takes longer than the specified duration"
            }
            LayerType::RateLimit => "Limits how often the tool can be called within a time window",
            LayerType::ConcurrencyLimit => {
                "Limits how many concurrent executions of this tool are allowed"
            }
        }
    }

    /// Get the config schema description for this layer.
    pub fn config_schema(&self) -> &'static str {
        match self {
            LayerType::Timeout => r#"{"secs": <number>} - timeout in seconds"#,
            LayerType::RateLimit => {
                r#"{"requests": <number>, "per_secs": <number>} - max requests per time window"#
            }
            LayerType::ConcurrencyLimit => r#"{"max": <number>} - maximum concurrent executions"#,
        }
    }
}

impl std::fmt::Display for LayerType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayerType::Timeout => write!(f, "timeout"),
            LayerType::RateLimit => write!(f, "rate_limit"),
            LayerType::ConcurrencyLimit => write!(f, "concurrency_limit"),
        }
    }
}

impl HandlerType {
    /// Get all available handler types.
    pub fn all() -> Vec<&'static str> {
        vec![
            "simple",
            "with_state",
            "with_context",
            "with_state_and_context",
            "raw",
            "no_params",
        ]
    }

    /// Get description for a handler type.
    pub fn description(&self) -> &'static str {
        match self {
            HandlerType::Simple => {
                "Basic handler that receives typed input: handler(|input: T| async { ... })"
            }
            HandlerType::WithState => {
                "Handler with shared state: handler_with_state(|state: Arc<S>, input: T| async { ... })"
            }
            HandlerType::WithContext => {
                "Handler with request context for progress/cancellation: handler_with_context(|ctx, input: T| async { ... })"
            }
            HandlerType::WithStateAndContext => {
                "Handler with both state and context: handler_with_state_and_context(|state, ctx, input: T| async { ... })"
            }
            HandlerType::Raw => {
                "Handler receiving raw JSON Value: raw_handler(|args: Value| async { ... })"
            }
            HandlerType::NoParams => {
                "Handler with no input parameters: no_params_handler(|| async { ... })"
            }
        }
    }
}

/// Available input field types.
pub struct InputTypes;

impl InputTypes {
    /// Get all available input types with descriptions.
    pub fn all() -> Vec<(&'static str, &'static str)> {
        vec![
            ("String", "UTF-8 string"),
            ("i64", "64-bit signed integer"),
            ("f64", "64-bit floating point"),
            ("bool", "Boolean true/false"),
            ("Option<String>", "Optional string (can be null)"),
            ("Option<i64>", "Optional integer"),
            ("Option<f64>", "Optional float"),
            ("Option<bool>", "Optional boolean"),
            ("Vec<String>", "Array of strings"),
            ("Vec<i64>", "Array of integers"),
            ("Value", "Raw JSON value (any structure)"),
        ]
    }
}
