//! Resource definition and builder API
//!
//! Provides ergonomic ways to define MCP resources:
//!
//! 1. **Builder pattern** - Fluent API for defining resources
//! 2. **Trait-based** - Implement `McpResource` for full control

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::error::Result;
use crate::protocol::{ReadResourceResult, ResourceContent, ResourceDefinition};

/// A boxed future for resource handlers
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Resource handler trait - the core abstraction for resource reading
pub trait ResourceHandler: Send + Sync {
    /// Read the resource contents
    fn read(&self) -> BoxFuture<'_, Result<ReadResourceResult>>;
}

/// A complete resource definition with handler
pub struct Resource {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
    handler: Arc<dyn ResourceHandler>,
}

impl Clone for Resource {
    fn clone(&self) -> Self {
        Self {
            uri: self.uri.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            mime_type: self.mime_type.clone(),
            handler: self.handler.clone(),
        }
    }
}

impl std::fmt::Debug for Resource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resource")
            .field("uri", &self.uri)
            .field("name", &self.name)
            .field("description", &self.description)
            .field("mime_type", &self.mime_type)
            .finish_non_exhaustive()
    }
}

impl Resource {
    /// Create a new resource builder
    pub fn builder(uri: impl Into<String>) -> ResourceBuilder {
        ResourceBuilder::new(uri)
    }

    /// Get the resource definition for resources/list
    pub fn definition(&self) -> ResourceDefinition {
        ResourceDefinition {
            uri: self.uri.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            mime_type: self.mime_type.clone(),
        }
    }

    /// Read the resource
    pub fn read(&self) -> BoxFuture<'_, Result<ReadResourceResult>> {
        self.handler.read()
    }
}

// =============================================================================
// Builder API
// =============================================================================

/// Builder for creating resources with a fluent API
///
/// # Example
///
/// ```rust
/// use tower_mcp::resource::ResourceBuilder;
/// use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
///
/// let resource = ResourceBuilder::new("file:///config.json")
///     .name("Configuration")
///     .description("Application configuration file")
///     .mime_type("application/json")
///     .handler(|| async {
///         Ok(ReadResourceResult {
///             contents: vec![ResourceContent {
///                 uri: "file:///config.json".to_string(),
///                 mime_type: Some("application/json".to_string()),
///                 text: Some(r#"{"setting": "value"}"#.to_string()),
///                 blob: None,
///             }],
///         })
///     });
///
/// assert_eq!(resource.uri, "file:///config.json");
/// ```
pub struct ResourceBuilder {
    uri: String,
    name: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
}

impl ResourceBuilder {
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            name: None,
            description: None,
            mime_type: None,
        }
    }

    /// Set the resource name (human-readable)
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set the resource description
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the MIME type of the resource
    pub fn mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = Some(mime_type.into());
        self
    }

    /// Set the handler function for reading the resource
    pub fn handler<F, Fut>(self, handler: F) -> Resource
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ReadResourceResult>> + Send + 'static,
    {
        // Default name to URI if not specified
        let name = self.name.unwrap_or_else(|| self.uri.clone());

        Resource {
            uri: self.uri.clone(),
            name,
            description: self.description,
            mime_type: self.mime_type,
            handler: Arc::new(FnHandler { handler }),
        }
    }

    /// Create a static text resource (convenience method)
    pub fn text(self, content: impl Into<String>) -> Resource {
        let uri = self.uri.clone();
        let content = content.into();
        let mime_type = self.mime_type.clone();

        self.handler(move || {
            let uri = uri.clone();
            let content = content.clone();
            let mime_type = mime_type.clone();
            async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type,
                        text: Some(content),
                        blob: None,
                    }],
                })
            }
        })
    }

    /// Create a static JSON resource (convenience method)
    pub fn json(mut self, value: serde_json::Value) -> Resource {
        let uri = self.uri.clone();
        self.mime_type = Some("application/json".to_string());
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string());

        self.handler(move || {
            let uri = uri.clone();
            let text = text.clone();
            async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: Some("application/json".to_string()),
                        text: Some(text),
                        blob: None,
                    }],
                })
            }
        })
    }
}

// =============================================================================
// Handler implementations
// =============================================================================

/// Handler wrapping a function
struct FnHandler<F> {
    handler: F,
}

impl<F, Fut> ResourceHandler for FnHandler<F>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ReadResourceResult>> + Send + 'static,
{
    fn read(&self) -> BoxFuture<'_, Result<ReadResourceResult>> {
        Box::pin((self.handler)())
    }
}

// =============================================================================
// Trait-based resource definition
// =============================================================================

/// Trait for defining resources with full control
///
/// Implement this trait when you need more control than the builder provides,
/// or when you want to define resources as standalone types.
///
/// # Example
///
/// ```rust
/// use tower_mcp::resource::McpResource;
/// use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
/// use tower_mcp::error::Result;
///
/// struct ConfigResource {
///     config: String,
/// }
///
/// impl McpResource for ConfigResource {
///     const URI: &'static str = "file:///config.json";
///     const NAME: &'static str = "Configuration";
///     const DESCRIPTION: Option<&'static str> = Some("Application configuration");
///     const MIME_TYPE: Option<&'static str> = Some("application/json");
///
///     async fn read(&self) -> Result<ReadResourceResult> {
///         Ok(ReadResourceResult {
///             contents: vec![ResourceContent {
///                 uri: Self::URI.to_string(),
///                 mime_type: Self::MIME_TYPE.map(|s| s.to_string()),
///                 text: Some(self.config.clone()),
///                 blob: None,
///             }],
///         })
///     }
/// }
///
/// let resource = ConfigResource { config: "{}".to_string() }.into_resource();
/// assert_eq!(resource.uri, "file:///config.json");
/// ```
pub trait McpResource: Send + Sync + 'static {
    const URI: &'static str;
    const NAME: &'static str;
    const DESCRIPTION: Option<&'static str> = None;
    const MIME_TYPE: Option<&'static str> = None;

    fn read(&self) -> impl Future<Output = Result<ReadResourceResult>> + Send;

    /// Convert to a Resource instance
    fn into_resource(self) -> Resource
    where
        Self: Sized,
    {
        let resource = Arc::new(self);
        Resource {
            uri: Self::URI.to_string(),
            name: Self::NAME.to_string(),
            description: Self::DESCRIPTION.map(|s| s.to_string()),
            mime_type: Self::MIME_TYPE.map(|s| s.to_string()),
            handler: Arc::new(McpResourceHandler { resource }),
        }
    }
}

/// Wrapper to make McpResource implement ResourceHandler
struct McpResourceHandler<T: McpResource> {
    resource: Arc<T>,
}

impl<T: McpResource> ResourceHandler for McpResourceHandler<T> {
    fn read(&self) -> BoxFuture<'_, Result<ReadResourceResult>> {
        let resource = self.resource.clone();
        Box::pin(async move { resource.read().await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_builder_resource() {
        let resource = ResourceBuilder::new("file:///test.txt")
            .name("Test File")
            .description("A test file")
            .text("Hello, World!");

        assert_eq!(resource.uri, "file:///test.txt");
        assert_eq!(resource.name, "Test File");
        assert_eq!(resource.description.as_deref(), Some("A test file"));

        let result = resource.read().await.unwrap();
        assert_eq!(result.contents.len(), 1);
        assert_eq!(result.contents[0].text.as_deref(), Some("Hello, World!"));
    }

    #[tokio::test]
    async fn test_json_resource() {
        let resource = ResourceBuilder::new("file:///config.json")
            .name("Config")
            .json(serde_json::json!({"key": "value"}));

        assert_eq!(resource.mime_type.as_deref(), Some("application/json"));

        let result = resource.read().await.unwrap();
        assert!(result.contents[0].text.as_ref().unwrap().contains("key"));
    }

    #[tokio::test]
    async fn test_handler_resource() {
        let resource = ResourceBuilder::new("memory://counter")
            .name("Counter")
            .handler(|| async {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: "memory://counter".to_string(),
                        mime_type: Some("text/plain".to_string()),
                        text: Some("42".to_string()),
                        blob: None,
                    }],
                })
            });

        let result = resource.read().await.unwrap();
        assert_eq!(result.contents[0].text.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn test_trait_resource() {
        struct TestResource;

        impl McpResource for TestResource {
            const URI: &'static str = "test://resource";
            const NAME: &'static str = "Test";
            const DESCRIPTION: Option<&'static str> = Some("A test resource");
            const MIME_TYPE: Option<&'static str> = Some("text/plain");

            async fn read(&self) -> Result<ReadResourceResult> {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: Self::URI.to_string(),
                        mime_type: Self::MIME_TYPE.map(|s| s.to_string()),
                        text: Some("test content".to_string()),
                        blob: None,
                    }],
                })
            }
        }

        let resource = TestResource.into_resource();
        assert_eq!(resource.uri, "test://resource");
        assert_eq!(resource.name, "Test");

        let result = resource.read().await.unwrap();
        assert_eq!(result.contents[0].text.as_deref(), Some("test content"));
    }

    #[test]
    fn test_resource_definition() {
        let resource = ResourceBuilder::new("file:///test.txt")
            .name("Test")
            .description("Description")
            .mime_type("text/plain")
            .text("content");

        let def = resource.definition();
        assert_eq!(def.uri, "file:///test.txt");
        assert_eq!(def.name, "Test");
        assert_eq!(def.description.as_deref(), Some("Description"));
        assert_eq!(def.mime_type.as_deref(), Some("text/plain"));
    }
}
