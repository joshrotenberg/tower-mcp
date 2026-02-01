//! Project state management for code generation.
//!
//! This module defines the data structures that accumulate project
//! configuration as the agent calls tools like `init_project`, `add_tool`, etc.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
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
