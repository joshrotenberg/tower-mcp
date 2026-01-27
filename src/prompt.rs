//! Prompt definition and builder API
//!
//! Provides ergonomic ways to define MCP prompts:
//!
//! 1. **Builder pattern** - Fluent API for defining prompts
//! 2. **Trait-based** - Implement `McpPrompt` for full control

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::error::Result;
use crate::protocol::{
    Content, GetPromptResult, PromptArgument, PromptDefinition, PromptMessage, PromptRole,
};

/// A boxed future for prompt handlers
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Prompt handler trait - the core abstraction for prompt generation
pub trait PromptHandler: Send + Sync {
    /// Get the prompt with the given arguments
    fn get(&self, arguments: HashMap<String, String>) -> BoxFuture<'_, Result<GetPromptResult>>;
}

/// A complete prompt definition with handler
pub struct Prompt {
    pub name: String,
    pub description: Option<String>,
    pub arguments: Vec<PromptArgument>,
    handler: Arc<dyn PromptHandler>,
}

impl Clone for Prompt {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            description: self.description.clone(),
            arguments: self.arguments.clone(),
            handler: self.handler.clone(),
        }
    }
}

impl std::fmt::Debug for Prompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prompt")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("arguments", &self.arguments)
            .finish_non_exhaustive()
    }
}

impl Prompt {
    /// Create a new prompt builder
    pub fn builder(name: impl Into<String>) -> PromptBuilder {
        PromptBuilder::new(name)
    }

    /// Get the prompt definition for prompts/list
    pub fn definition(&self) -> PromptDefinition {
        PromptDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            arguments: self.arguments.clone(),
        }
    }

    /// Get the prompt with arguments
    pub fn get(
        &self,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<GetPromptResult>> {
        self.handler.get(arguments)
    }
}

// =============================================================================
// Builder API
// =============================================================================

/// Builder for creating prompts with a fluent API
///
/// # Example
///
/// ```rust
/// use tower_mcp::prompt::PromptBuilder;
/// use tower_mcp::protocol::{GetPromptResult, PromptMessage, PromptRole, Content};
///
/// let prompt = PromptBuilder::new("greet")
///     .description("Generate a greeting")
///     .required_arg("name", "The name to greet")
///     .handler(|args| async move {
///         let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
///         Ok(GetPromptResult {
///             description: Some("A greeting prompt".to_string()),
///             messages: vec![PromptMessage {
///                 role: PromptRole::User,
///                 content: Content::Text {
///                     text: format!("Please greet {}", name),
///                     annotations: None,
///                 },
///             }],
///         })
///     });
///
/// assert_eq!(prompt.name, "greet");
/// ```
pub struct PromptBuilder {
    name: String,
    description: Option<String>,
    arguments: Vec<PromptArgument>,
}

impl PromptBuilder {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            arguments: Vec::new(),
        }
    }

    /// Set the prompt description
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add a required argument
    pub fn required_arg(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.arguments.push(PromptArgument {
            name: name.into(),
            description: Some(description.into()),
            required: true,
        });
        self
    }

    /// Add an optional argument
    pub fn optional_arg(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.arguments.push(PromptArgument {
            name: name.into(),
            description: Some(description.into()),
            required: false,
        });
        self
    }

    /// Add an argument with full control
    pub fn argument(mut self, arg: PromptArgument) -> Self {
        self.arguments.push(arg);
        self
    }

    /// Set the handler function for getting the prompt
    pub fn handler<F, Fut>(self, handler: F) -> Prompt
    where
        F: Fn(HashMap<String, String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<GetPromptResult>> + Send + 'static,
    {
        Prompt {
            name: self.name,
            description: self.description,
            arguments: self.arguments,
            handler: Arc::new(FnHandler { handler }),
        }
    }

    /// Create a static prompt (no arguments needed)
    pub fn static_prompt(self, messages: Vec<PromptMessage>) -> Prompt {
        let description = self.description.clone();
        self.handler(move |_| {
            let messages = messages.clone();
            let description = description.clone();
            async move {
                Ok(GetPromptResult {
                    description,
                    messages,
                })
            }
        })
    }

    /// Create a simple text prompt with a user message
    pub fn user_message(self, text: impl Into<String>) -> Prompt {
        let text = text.into();
        self.static_prompt(vec![PromptMessage {
            role: PromptRole::User,
            content: Content::Text {
                text,
                annotations: None,
            },
        }])
    }

    /// Finalize the builder into a Prompt
    ///
    /// This is an alias for `handler` for when you want to explicitly mark the build step.
    pub fn build<F, Fut>(self, handler: F) -> Prompt
    where
        F: Fn(HashMap<String, String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<GetPromptResult>> + Send + 'static,
    {
        self.handler(handler)
    }
}

// =============================================================================
// Handler implementations
// =============================================================================

/// Handler wrapping a function
struct FnHandler<F> {
    handler: F,
}

impl<F, Fut> PromptHandler for FnHandler<F>
where
    F: Fn(HashMap<String, String>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<GetPromptResult>> + Send + 'static,
{
    fn get(&self, arguments: HashMap<String, String>) -> BoxFuture<'_, Result<GetPromptResult>> {
        Box::pin((self.handler)(arguments))
    }
}

// =============================================================================
// Trait-based prompt definition
// =============================================================================

/// Trait for defining prompts with full control
///
/// Implement this trait when you need more control than the builder provides,
/// or when you want to define prompts as standalone types.
///
/// # Example
///
/// ```rust
/// use std::collections::HashMap;
/// use tower_mcp::prompt::McpPrompt;
/// use tower_mcp::protocol::{GetPromptResult, PromptArgument, PromptMessage, PromptRole, Content};
/// use tower_mcp::error::Result;
///
/// struct CodeReviewPrompt;
///
/// impl McpPrompt for CodeReviewPrompt {
///     const NAME: &'static str = "code_review";
///     const DESCRIPTION: &'static str = "Review code for issues";
///
///     fn arguments(&self) -> Vec<PromptArgument> {
///         vec![
///             PromptArgument {
///                 name: "code".to_string(),
///                 description: Some("The code to review".to_string()),
///                 required: true,
///             },
///             PromptArgument {
///                 name: "language".to_string(),
///                 description: Some("Programming language".to_string()),
///                 required: false,
///             },
///         ]
///     }
///
///     async fn get(&self, args: HashMap<String, String>) -> Result<GetPromptResult> {
///         let code = args.get("code").map(|s| s.as_str()).unwrap_or("");
///         let lang = args.get("language").map(|s| s.as_str()).unwrap_or("unknown");
///
///         Ok(GetPromptResult {
///             description: Some("Code review prompt".to_string()),
///             messages: vec![PromptMessage {
///                 role: PromptRole::User,
///                 content: Content::Text {
///                     text: format!("Please review this {} code:\n\n```{}\n{}\n```", lang, lang, code),
///                     annotations: None,
///                 },
///             }],
///         })
///     }
/// }
///
/// let prompt = CodeReviewPrompt.into_prompt();
/// assert_eq!(prompt.name, "code_review");
/// ```
pub trait McpPrompt: Send + Sync + 'static {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;

    /// Define the arguments for this prompt
    fn arguments(&self) -> Vec<PromptArgument> {
        Vec::new()
    }

    fn get(
        &self,
        arguments: HashMap<String, String>,
    ) -> impl Future<Output = Result<GetPromptResult>> + Send;

    /// Convert to a Prompt instance
    fn into_prompt(self) -> Prompt
    where
        Self: Sized,
    {
        let arguments = self.arguments();
        let prompt = Arc::new(self);
        Prompt {
            name: Self::NAME.to_string(),
            description: Some(Self::DESCRIPTION.to_string()),
            arguments,
            handler: Arc::new(McpPromptHandler { prompt }),
        }
    }
}

/// Wrapper to make McpPrompt implement PromptHandler
struct McpPromptHandler<T: McpPrompt> {
    prompt: Arc<T>,
}

impl<T: McpPrompt> PromptHandler for McpPromptHandler<T> {
    fn get(&self, arguments: HashMap<String, String>) -> BoxFuture<'_, Result<GetPromptResult>> {
        let prompt = self.prompt.clone();
        Box::pin(async move { prompt.get(arguments).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_builder_prompt() {
        let prompt = PromptBuilder::new("greet")
            .description("A greeting prompt")
            .required_arg("name", "Name to greet")
            .handler(|args| async move {
                let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
                Ok(GetPromptResult {
                    description: Some("Greeting".to_string()),
                    messages: vec![PromptMessage {
                        role: PromptRole::User,
                        content: Content::Text {
                            text: format!("Hello, {}!", name),
                            annotations: None,
                        },
                    }],
                })
            });

        assert_eq!(prompt.name, "greet");
        assert_eq!(prompt.description.as_deref(), Some("A greeting prompt"));
        assert_eq!(prompt.arguments.len(), 1);
        assert!(prompt.arguments[0].required);

        let mut args = HashMap::new();
        args.insert("name".to_string(), "Alice".to_string());
        let result = prompt.get(args).await.unwrap();

        assert_eq!(result.messages.len(), 1);
        match &result.messages[0].content {
            Content::Text { text, .. } => assert_eq!(text, "Hello, Alice!"),
            _ => panic!("Expected text content"),
        }
    }

    #[tokio::test]
    async fn test_static_prompt() {
        let prompt = PromptBuilder::new("help")
            .description("Help prompt")
            .user_message("How can I help you today?");

        let result = prompt.get(HashMap::new()).await.unwrap();
        assert_eq!(result.messages.len(), 1);
        match &result.messages[0].content {
            Content::Text { text, .. } => assert_eq!(text, "How can I help you today?"),
            _ => panic!("Expected text content"),
        }
    }

    #[tokio::test]
    async fn test_trait_prompt() {
        struct TestPrompt;

        impl McpPrompt for TestPrompt {
            const NAME: &'static str = "test";
            const DESCRIPTION: &'static str = "A test prompt";

            fn arguments(&self) -> Vec<PromptArgument> {
                vec![PromptArgument {
                    name: "input".to_string(),
                    description: Some("Test input".to_string()),
                    required: true,
                }]
            }

            async fn get(&self, args: HashMap<String, String>) -> Result<GetPromptResult> {
                let input = args.get("input").map(|s| s.as_str()).unwrap_or("default");
                Ok(GetPromptResult {
                    description: Some("Test".to_string()),
                    messages: vec![PromptMessage {
                        role: PromptRole::User,
                        content: Content::Text {
                            text: format!("Input: {}", input),
                            annotations: None,
                        },
                    }],
                })
            }
        }

        let prompt = TestPrompt.into_prompt();
        assert_eq!(prompt.name, "test");
        assert_eq!(prompt.arguments.len(), 1);

        let mut args = HashMap::new();
        args.insert("input".to_string(), "hello".to_string());
        let result = prompt.get(args).await.unwrap();

        match &result.messages[0].content {
            Content::Text { text, .. } => assert_eq!(text, "Input: hello"),
            _ => panic!("Expected text content"),
        }
    }

    #[test]
    fn test_prompt_definition() {
        let prompt = PromptBuilder::new("test")
            .description("Test description")
            .required_arg("arg1", "First arg")
            .optional_arg("arg2", "Second arg")
            .user_message("Test");

        let def = prompt.definition();
        assert_eq!(def.name, "test");
        assert_eq!(def.description.as_deref(), Some("Test description"));
        assert_eq!(def.arguments.len(), 2);
        assert!(def.arguments[0].required);
        assert!(!def.arguments[1].required);
    }
}
