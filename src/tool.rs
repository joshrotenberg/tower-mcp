//! Tool definition and builder API
//!
//! Provides ergonomic ways to define MCP tools:
//!
//! 1. **Builder pattern** - Fluent API for defining tools
//! 2. **Trait-based** - Implement `McpTool` for full control
//! 3. **Function-based** - Quick tools from async functions

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::protocol::{CallToolResult, ToolAnnotations, ToolDefinition};

/// Validate a tool name according to MCP spec.
///
/// Tool names must be:
/// - 1-128 characters long
/// - Contain only alphanumeric characters, underscores, hyphens, and dots
///
/// Returns `Ok(())` if valid, `Err` with description if invalid.
pub fn validate_tool_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::tool("Tool name cannot be empty"));
    }
    if name.len() > 128 {
        return Err(Error::tool(format!(
            "Tool name '{}' exceeds maximum length of 128 characters (got {})",
            name,
            name.len()
        )));
    }
    if let Some(invalid_char) = name
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && *c != '_' && *c != '-' && *c != '.')
    {
        return Err(Error::tool(format!(
            "Tool name '{}' contains invalid character '{}'. Only alphanumeric, underscore, hyphen, and dot are allowed.",
            name, invalid_char
        )));
    }
    Ok(())
}

/// A boxed future for tool handlers
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Tool handler trait - the core abstraction for tool execution
pub trait ToolHandler: Send + Sync {
    /// Execute the tool with the given arguments
    fn call(&self, args: Value) -> BoxFuture<'_, Result<CallToolResult>>;

    /// Get the tool's input schema
    fn input_schema(&self) -> Value;
}

/// A complete tool definition with handler
pub struct Tool {
    pub name: String,
    pub description: Option<String>,
    pub annotations: Option<ToolAnnotations>,
    handler: Arc<dyn ToolHandler>,
}

impl std::fmt::Debug for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tool")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("annotations", &self.annotations)
            .finish_non_exhaustive()
    }
}

impl Tool {
    /// Create a new tool builder
    pub fn builder(name: impl Into<String>) -> ToolBuilder {
        ToolBuilder::new(name)
    }

    /// Get the tool definition for tools/list
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.handler.input_schema(),
            annotations: self.annotations.clone(),
        }
    }

    /// Call the tool
    pub fn call(&self, args: Value) -> BoxFuture<'_, Result<CallToolResult>> {
        self.handler.call(args)
    }
}

// =============================================================================
// Builder API
// =============================================================================

/// Builder for creating tools with a fluent API
///
/// # Example
///
/// ```rust
/// use tower_mcp::{ToolBuilder, CallToolResult};
/// use schemars::JsonSchema;
/// use serde::Deserialize;
///
/// #[derive(Debug, Deserialize, JsonSchema)]
/// struct GreetInput {
///     name: String,
/// }
///
/// let tool = ToolBuilder::new("greet")
///     .description("Greet someone by name")
///     .handler(|input: GreetInput| async move {
///         Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
///     })
///     .build()
///     .expect("valid tool name");
///
/// assert_eq!(tool.name, "greet");
/// ```
pub struct ToolBuilder {
    name: String,
    description: Option<String>,
    annotations: Option<ToolAnnotations>,
}

impl ToolBuilder {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            annotations: None,
        }
    }

    /// Set a human-readable title for the tool (stored in annotations)
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.annotations
            .get_or_insert_with(ToolAnnotations::default)
            .title = Some(title.into());
        self
    }

    /// Set the tool description
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Mark the tool as read-only (does not modify state)
    pub fn read_only(mut self) -> Self {
        self.annotations
            .get_or_insert_with(ToolAnnotations::default)
            .read_only_hint = true;
        self
    }

    /// Mark the tool as non-destructive
    pub fn non_destructive(mut self) -> Self {
        self.annotations
            .get_or_insert_with(ToolAnnotations::default)
            .destructive_hint = false;
        self
    }

    /// Mark the tool as idempotent (same args = same effect)
    pub fn idempotent(mut self) -> Self {
        self.annotations
            .get_or_insert_with(ToolAnnotations::default)
            .idempotent_hint = true;
        self
    }

    /// Set tool annotations directly
    pub fn annotations(mut self, annotations: ToolAnnotations) -> Self {
        self.annotations = Some(annotations);
        self
    }

    /// Specify input type and handler
    ///
    /// The input type must implement `JsonSchema` and `DeserializeOwned`.
    /// The handler receives the deserialized input and returns a `CallToolResult`.
    pub fn handler<I, F, Fut>(self, handler: F) -> ToolBuilderWithHandler<I, F>
    where
        I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
        F: Fn(I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<CallToolResult>> + Send + 'static,
    {
        ToolBuilderWithHandler {
            name: self.name,
            description: self.description,
            annotations: self.annotations,
            handler,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Create a tool with raw JSON handling (no automatic deserialization)
    ///
    /// Returns an error if the tool name is invalid.
    pub fn raw_handler<F, Fut>(self, handler: F) -> Result<Tool>
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<CallToolResult>> + Send + 'static,
    {
        validate_tool_name(&self.name)?;
        Ok(Tool {
            name: self.name,
            description: self.description,
            annotations: self.annotations,
            handler: Arc::new(RawHandler { handler }),
        })
    }
}

/// Builder state after handler is specified
pub struct ToolBuilderWithHandler<I, F> {
    name: String,
    description: Option<String>,
    annotations: Option<ToolAnnotations>,
    handler: F,
    _phantom: std::marker::PhantomData<I>,
}

impl<I, F, Fut> ToolBuilderWithHandler<I, F>
where
    I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
    F: Fn(I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send + 'static,
{
    /// Build the tool
    ///
    /// Returns an error if the tool name is invalid.
    pub fn build(self) -> Result<Tool> {
        validate_tool_name(&self.name)?;
        Ok(Tool {
            name: self.name,
            description: self.description,
            annotations: self.annotations,
            handler: Arc::new(TypedHandler {
                handler: self.handler,
                _phantom: std::marker::PhantomData,
            }),
        })
    }
}

// =============================================================================
// Handler implementations
// =============================================================================

/// Handler that deserializes input to a specific type
struct TypedHandler<I, F> {
    handler: F,
    _phantom: std::marker::PhantomData<I>,
}

impl<I, F, Fut> ToolHandler for TypedHandler<I, F>
where
    I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
    F: Fn(I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send + 'static,
{
    fn call(&self, args: Value) -> BoxFuture<'_, Result<CallToolResult>> {
        Box::pin(async move {
            let input: I = serde_json::from_value(args)
                .map_err(|e| Error::tool(format!("Invalid input: {}", e)))?;
            (self.handler)(input).await
        })
    }

    fn input_schema(&self) -> Value {
        let schema = schemars::schema_for!(I);
        serde_json::to_value(schema).unwrap_or_else(|_| {
            serde_json::json!({
                "type": "object"
            })
        })
    }
}

/// Handler that works with raw JSON
struct RawHandler<F> {
    handler: F,
}

impl<F, Fut> ToolHandler for RawHandler<F>
where
    F: Fn(Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send + 'static,
{
    fn call(&self, args: Value) -> BoxFuture<'_, Result<CallToolResult>> {
        Box::pin((self.handler)(args))
    }

    fn input_schema(&self) -> Value {
        // Raw handlers accept any JSON
        serde_json::json!({
            "type": "object",
            "additionalProperties": true
        })
    }
}

// =============================================================================
// Trait-based tool definition
// =============================================================================

/// Trait for defining tools with full control
///
/// Implement this trait when you need more control than the builder provides,
/// or when you want to define tools as standalone types.
///
/// # Example
///
/// ```rust
/// use tower_mcp::tool::McpTool;
/// use tower_mcp::error::Result;
/// use schemars::JsonSchema;
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Debug, Deserialize, JsonSchema)]
/// struct AddInput {
///     a: i64,
///     b: i64,
/// }
///
/// struct AddTool;
///
/// impl McpTool for AddTool {
///     const NAME: &'static str = "add";
///     const DESCRIPTION: &'static str = "Add two numbers";
///
///     type Input = AddInput;
///     type Output = i64;
///
///     async fn call(&self, input: Self::Input) -> Result<Self::Output> {
///         Ok(input.a + input.b)
///     }
/// }
///
/// let tool = AddTool.into_tool().expect("valid tool name");
/// assert_eq!(tool.name, "add");
/// ```
pub trait McpTool: Send + Sync + 'static {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;

    type Input: JsonSchema + DeserializeOwned + Send;
    type Output: Serialize + Send;

    fn call(&self, input: Self::Input) -> impl Future<Output = Result<Self::Output>> + Send;

    /// Optional annotations for the tool
    fn annotations(&self) -> Option<ToolAnnotations> {
        None
    }

    /// Convert to a Tool instance
    ///
    /// Returns an error if the tool name is invalid.
    fn into_tool(self) -> Result<Tool>
    where
        Self: Sized,
    {
        validate_tool_name(Self::NAME)?;
        let annotations = self.annotations();
        let tool = Arc::new(self);
        Ok(Tool {
            name: Self::NAME.to_string(),
            description: Some(Self::DESCRIPTION.to_string()),
            annotations,
            handler: Arc::new(McpToolHandler { tool }),
        })
    }
}

/// Wrapper to make McpTool implement ToolHandler
struct McpToolHandler<T: McpTool> {
    tool: Arc<T>,
}

impl<T: McpTool> ToolHandler for McpToolHandler<T> {
    fn call(&self, args: Value) -> BoxFuture<'_, Result<CallToolResult>> {
        let tool = self.tool.clone();
        Box::pin(async move {
            let input: T::Input = serde_json::from_value(args)
                .map_err(|e| Error::tool(format!("Invalid input: {}", e)))?;
            let output = tool.call(input).await?;
            let value = serde_json::to_value(output)
                .map_err(|e| Error::tool(format!("Failed to serialize output: {}", e)))?;
            Ok(CallToolResult::json(value))
        })
    }

    fn input_schema(&self) -> Value {
        let schema = schemars::schema_for!(T::Input);
        serde_json::to_value(schema).unwrap_or_else(|_| {
            serde_json::json!({
                "type": "object"
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct GreetInput {
        name: String,
    }

    #[tokio::test]
    async fn test_builder_tool() {
        let tool = ToolBuilder::new("greet")
            .description("Greet someone")
            .handler(|input: GreetInput| async move {
                Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
            })
            .build()
            .expect("valid tool name");

        assert_eq!(tool.name, "greet");
        assert_eq!(tool.description.as_deref(), Some("Greet someone"));

        let result = tool
            .call(serde_json::json!({"name": "World"}))
            .await
            .unwrap();

        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn test_raw_handler() {
        let tool = ToolBuilder::new("echo")
            .description("Echo input")
            .raw_handler(|args: Value| async move { Ok(CallToolResult::json(args)) })
            .expect("valid tool name");

        let result = tool.call(serde_json::json!({"foo": "bar"})).await.unwrap();

        assert!(!result.is_error);
    }

    #[test]
    fn test_invalid_tool_name_empty() {
        let result = ToolBuilder::new("")
            .description("Empty name")
            .raw_handler(|args: Value| async move { Ok(CallToolResult::json(args)) });

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));
    }

    #[test]
    fn test_invalid_tool_name_too_long() {
        let long_name = "a".repeat(129);
        let result = ToolBuilder::new(long_name)
            .description("Too long")
            .raw_handler(|args: Value| async move { Ok(CallToolResult::json(args)) });

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exceeds maximum"));
    }

    #[test]
    fn test_invalid_tool_name_bad_chars() {
        let result = ToolBuilder::new("my tool!")
            .description("Bad chars")
            .raw_handler(|args: Value| async move { Ok(CallToolResult::json(args)) });

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid character")
        );
    }

    #[test]
    fn test_valid_tool_names() {
        // All valid characters
        let names = [
            "my_tool",
            "my-tool",
            "my.tool",
            "MyTool123",
            "a",
            &"a".repeat(128),
        ];
        for name in names {
            let result = ToolBuilder::new(name)
                .description("Valid")
                .raw_handler(|args: Value| async move { Ok(CallToolResult::json(args)) });
            assert!(result.is_ok(), "Expected '{}' to be valid", name);
        }
    }
}
