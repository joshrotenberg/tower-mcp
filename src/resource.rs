//! Resource definition and builder API
//!
//! Provides ergonomic ways to define MCP resources:
//!
//! 1. **Builder pattern** - Fluent API for defining resources
//! 2. **Trait-based** - Implement `McpResource` for full control
//! 3. **Resource templates** - Parameterized resources using URI templates (RFC 6570)
//!
//! # Resource Templates
//!
//! Resource templates allow servers to expose parameterized resources using URI templates.
//! When a client requests `resources/read` with a URI matching a template, the server
//! extracts the variables and passes them to the handler.
//!
//! ```rust
//! use tower_mcp::resource::ResourceTemplateBuilder;
//! use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
//! use std::collections::HashMap;
//!
//! let template = ResourceTemplateBuilder::new("file:///{path}")
//!     .name("Project Files")
//!     .description("Access files in the project directory")
//!     .handler(|uri: String, vars: HashMap<String, String>| async move {
//!         let path = vars.get("path").unwrap_or(&String::new()).clone();
//!         Ok(ReadResourceResult {
//!             contents: vec![ResourceContent {
//!                 uri,
//!                 mime_type: Some("text/plain".to_string()),
//!                 text: Some(format!("Contents of {}", path)),
//!                 blob: None,
//!             }],
//!         })
//!     });
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::error::Result;
use crate::protocol::{
    ReadResourceResult, ResourceContent, ResourceDefinition, ResourceTemplateDefinition,
};

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

// =============================================================================
// Resource Templates
// =============================================================================

/// Handler trait for resource templates
///
/// Unlike [`ResourceHandler`], template handlers receive the extracted
/// URI variables as a parameter.
pub trait ResourceTemplateHandler: Send + Sync {
    /// Read a resource with the given URI variables extracted from the template
    fn read(
        &self,
        uri: &str,
        variables: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<ReadResourceResult>>;
}

/// A parameterized resource template
///
/// Resource templates use URI template syntax (RFC 6570) to match multiple URIs
/// and extract variable values. This allows servers to expose dynamic resources
/// like file systems or database records.
///
/// # Example
///
/// ```rust
/// use tower_mcp::resource::ResourceTemplateBuilder;
/// use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
/// use std::collections::HashMap;
///
/// let template = ResourceTemplateBuilder::new("file:///{path}")
///     .name("Project Files")
///     .handler(|uri: String, vars: HashMap<String, String>| async move {
///         let path = vars.get("path").unwrap_or(&String::new()).clone();
///         Ok(ReadResourceResult {
///             contents: vec![ResourceContent {
///                 uri,
///                 mime_type: Some("text/plain".to_string()),
///                 text: Some(format!("Contents of {}", path)),
///                 blob: None,
///             }],
///         })
///     });
/// ```
pub struct ResourceTemplate {
    /// The URI template pattern (e.g., `file:///{path}`)
    pub uri_template: String,
    /// Human-readable name
    pub name: String,
    /// Optional description
    pub description: Option<String>,
    /// Optional MIME type hint
    pub mime_type: Option<String>,
    /// Compiled regex for matching URIs
    pattern: regex::Regex,
    /// Variable names in order of appearance
    variables: Vec<String>,
    /// Handler for reading matched resources
    handler: Arc<dyn ResourceTemplateHandler>,
}

impl Clone for ResourceTemplate {
    fn clone(&self) -> Self {
        Self {
            uri_template: self.uri_template.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            mime_type: self.mime_type.clone(),
            pattern: self.pattern.clone(),
            variables: self.variables.clone(),
            handler: self.handler.clone(),
        }
    }
}

impl std::fmt::Debug for ResourceTemplate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceTemplate")
            .field("uri_template", &self.uri_template)
            .field("name", &self.name)
            .field("description", &self.description)
            .field("mime_type", &self.mime_type)
            .field("variables", &self.variables)
            .finish_non_exhaustive()
    }
}

impl ResourceTemplate {
    /// Create a new resource template builder
    pub fn builder(uri_template: impl Into<String>) -> ResourceTemplateBuilder {
        ResourceTemplateBuilder::new(uri_template)
    }

    /// Get the template definition for resources/templates/list
    pub fn definition(&self) -> ResourceTemplateDefinition {
        ResourceTemplateDefinition {
            uri_template: self.uri_template.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            mime_type: self.mime_type.clone(),
        }
    }

    /// Check if a URI matches this template and extract variables
    ///
    /// Returns `Some(HashMap)` with extracted variables if the URI matches,
    /// `None` if it doesn't match.
    pub fn match_uri(&self, uri: &str) -> Option<HashMap<String, String>> {
        self.pattern.captures(uri).map(|caps| {
            self.variables
                .iter()
                .enumerate()
                .filter_map(|(i, name)| {
                    caps.get(i + 1)
                        .map(|m| (name.clone(), m.as_str().to_string()))
                })
                .collect()
        })
    }

    /// Read a resource at the given URI using this template's handler
    ///
    /// # Arguments
    ///
    /// * `uri` - The actual URI being read
    /// * `variables` - Variables extracted from matching the URI against the template
    pub fn read(
        &self,
        uri: &str,
        variables: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<ReadResourceResult>> {
        self.handler.read(uri, variables)
    }
}

/// Builder for creating resource templates
///
/// # Example
///
/// ```rust
/// use tower_mcp::resource::ResourceTemplateBuilder;
/// use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
/// use std::collections::HashMap;
///
/// let template = ResourceTemplateBuilder::new("db://users/{id}")
///     .name("User Records")
///     .description("Access user records by ID")
///     .handler(|uri: String, vars: HashMap<String, String>| async move {
///         let id = vars.get("id").unwrap();
///         Ok(ReadResourceResult {
///             contents: vec![ResourceContent {
///                 uri,
///                 mime_type: Some("application/json".to_string()),
///                 text: Some(format!(r#"{{"id": "{}"}}"#, id)),
///                 blob: None,
///             }],
///         })
///     });
/// ```
pub struct ResourceTemplateBuilder {
    uri_template: String,
    name: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
}

impl ResourceTemplateBuilder {
    /// Create a new builder with the given URI template
    ///
    /// # URI Template Syntax
    ///
    /// Templates use RFC 6570 Level 1 syntax with simple variable expansion:
    /// - `{varname}` - Matches any non-slash characters
    ///
    /// # Examples
    ///
    /// - `file:///{path}` - Matches `file:///README.md`
    /// - `db://users/{id}` - Matches `db://users/123`
    /// - `api://v1/{resource}/{id}` - Matches `api://v1/posts/456`
    pub fn new(uri_template: impl Into<String>) -> Self {
        Self {
            uri_template: uri_template.into(),
            name: None,
            description: None,
            mime_type: None,
        }
    }

    /// Set the human-readable name for this template
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set the description for this template
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the MIME type hint for resources from this template
    pub fn mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = Some(mime_type.into());
        self
    }

    /// Set the handler function for reading template resources
    ///
    /// The handler receives:
    /// - `uri`: The full URI being read
    /// - `variables`: A map of variable names to their values extracted from the URI
    pub fn handler<F, Fut>(self, handler: F) -> ResourceTemplate
    where
        F: Fn(String, HashMap<String, String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ReadResourceResult>> + Send + 'static,
    {
        let (pattern, variables) = compile_uri_template(&self.uri_template);
        let name = self.name.unwrap_or_else(|| self.uri_template.clone());

        ResourceTemplate {
            uri_template: self.uri_template,
            name,
            description: self.description,
            mime_type: self.mime_type,
            pattern,
            variables,
            handler: Arc::new(FnTemplateHandler { handler }),
        }
    }
}

/// Handler wrapping a function for templates
struct FnTemplateHandler<F> {
    handler: F,
}

impl<F, Fut> ResourceTemplateHandler for FnTemplateHandler<F>
where
    F: Fn(String, HashMap<String, String>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ReadResourceResult>> + Send + 'static,
{
    fn read(
        &self,
        uri: &str,
        variables: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<ReadResourceResult>> {
        let uri = uri.to_string();
        Box::pin((self.handler)(uri, variables))
    }
}

/// Compile a URI template into a regex pattern and extract variable names
///
/// Supports RFC 6570 Level 1 (simple expansion):
/// - `{var}` matches any characters except `/`
/// - `{+var}` matches any characters including `/` (reserved expansion)
///
/// Returns the compiled regex and a list of variable names in order.
fn compile_uri_template(template: &str) -> (regex::Regex, Vec<String>) {
    let mut pattern = String::from("^");
    let mut variables = Vec::new();

    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            // Check for + prefix (reserved expansion)
            let is_reserved = chars.peek() == Some(&'+');
            if is_reserved {
                chars.next();
            }

            // Collect variable name
            let var_name: String = chars.by_ref().take_while(|&c| c != '}').collect();
            variables.push(var_name);

            // Choose pattern based on expansion type
            if is_reserved {
                // Reserved expansion - match anything
                pattern.push_str("(.+)");
            } else {
                // Simple expansion - match non-slash characters
                pattern.push_str("([^/]+)");
            }
        } else {
            // Escape regex special characters
            match c {
                '.' | '+' | '*' | '?' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
                | '\\' => {
                    pattern.push('\\');
                    pattern.push(c);
                }
                _ => pattern.push(c),
            }
        }
    }

    pattern.push('$');

    // Compile the regex - panic if template is malformed
    let regex = regex::Regex::new(&pattern)
        .unwrap_or_else(|e| panic!("Invalid URI template '{}': {}", template, e));

    (regex, variables)
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

    // =========================================================================
    // Resource Template Tests
    // =========================================================================

    #[test]
    fn test_compile_uri_template_simple() {
        let (regex, vars) = compile_uri_template("file:///{path}");
        assert_eq!(vars, vec!["path"]);
        assert!(regex.is_match("file:///README.md"));
        assert!(!regex.is_match("file:///foo/bar")); // no slashes in simple expansion
    }

    #[test]
    fn test_compile_uri_template_multiple_vars() {
        let (regex, vars) = compile_uri_template("api://v1/{resource}/{id}");
        assert_eq!(vars, vec!["resource", "id"]);
        assert!(regex.is_match("api://v1/users/123"));
        assert!(regex.is_match("api://v1/posts/abc"));
        assert!(!regex.is_match("api://v1/users")); // missing id
    }

    #[test]
    fn test_compile_uri_template_reserved_expansion() {
        let (regex, vars) = compile_uri_template("file:///{+path}");
        assert_eq!(vars, vec!["path"]);
        assert!(regex.is_match("file:///README.md"));
        assert!(regex.is_match("file:///foo/bar/baz.txt")); // slashes allowed
    }

    #[test]
    fn test_compile_uri_template_special_chars() {
        let (regex, vars) = compile_uri_template("http://example.com/api?query={q}");
        assert_eq!(vars, vec!["q"]);
        assert!(regex.is_match("http://example.com/api?query=hello"));
    }

    #[test]
    fn test_resource_template_match_uri() {
        let template = ResourceTemplateBuilder::new("db://users/{id}")
            .name("User Records")
            .handler(|uri: String, vars: HashMap<String, String>| async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: None,
                        text: Some(format!("User {}", vars.get("id").unwrap())),
                        blob: None,
                    }],
                })
            });

        // Test matching
        let vars = template.match_uri("db://users/123").unwrap();
        assert_eq!(vars.get("id"), Some(&"123".to_string()));

        // Test non-matching
        assert!(template.match_uri("db://posts/123").is_none());
        assert!(template.match_uri("db://users").is_none());
    }

    #[test]
    fn test_resource_template_match_multiple_vars() {
        let template = ResourceTemplateBuilder::new("api://{version}/{resource}/{id}")
            .name("API Resources")
            .handler(|uri: String, _vars: HashMap<String, String>| async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: None,
                        text: None,
                        blob: None,
                    }],
                })
            });

        let vars = template.match_uri("api://v2/users/abc-123").unwrap();
        assert_eq!(vars.get("version"), Some(&"v2".to_string()));
        assert_eq!(vars.get("resource"), Some(&"users".to_string()));
        assert_eq!(vars.get("id"), Some(&"abc-123".to_string()));
    }

    #[tokio::test]
    async fn test_resource_template_read() {
        let template = ResourceTemplateBuilder::new("file:///{path}")
            .name("Files")
            .mime_type("text/plain")
            .handler(|uri: String, vars: HashMap<String, String>| async move {
                let path = vars.get("path").unwrap().clone();
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: Some("text/plain".to_string()),
                        text: Some(format!("Contents of {}", path)),
                        blob: None,
                    }],
                })
            });

        let vars = template.match_uri("file:///README.md").unwrap();
        let result = template.read("file:///README.md", vars).await.unwrap();

        assert_eq!(result.contents.len(), 1);
        assert_eq!(result.contents[0].uri, "file:///README.md");
        assert_eq!(
            result.contents[0].text.as_deref(),
            Some("Contents of README.md")
        );
    }

    #[test]
    fn test_resource_template_definition() {
        let template = ResourceTemplateBuilder::new("db://records/{id}")
            .name("Database Records")
            .description("Access database records by ID")
            .mime_type("application/json")
            .handler(|uri: String, _vars: HashMap<String, String>| async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: None,
                        text: None,
                        blob: None,
                    }],
                })
            });

        let def = template.definition();
        assert_eq!(def.uri_template, "db://records/{id}");
        assert_eq!(def.name, "Database Records");
        assert_eq!(
            def.description.as_deref(),
            Some("Access database records by ID")
        );
        assert_eq!(def.mime_type.as_deref(), Some("application/json"));
    }

    #[test]
    fn test_resource_template_reserved_path() {
        let template = ResourceTemplateBuilder::new("file:///{+path}")
            .name("Files with subpaths")
            .handler(|uri: String, _vars: HashMap<String, String>| async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: None,
                        text: None,
                        blob: None,
                    }],
                })
            });

        // Reserved expansion should match slashes
        let vars = template.match_uri("file:///src/lib/utils.rs").unwrap();
        assert_eq!(vars.get("path"), Some(&"src/lib/utils.rs".to_string()));
    }
}
