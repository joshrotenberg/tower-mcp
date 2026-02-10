# Dynamic Tool Registry Design for tower-mcp

RFC for runtime tool registration and composite tool creation.

## Motivation

Enable agents to create tools at runtime:
1. **Workflow composition** - bundle repetitive tool sequences into new tools
2. **Task-specific tooling** - agent creates tools tailored to current task
3. **Learning/adaptation** - agent improves its own capabilities over time

## Core Primitives

### 1. DynamicToolRegistry

A thread-safe registry that allows runtime tool registration.

```rust
use std::sync::Arc;
use tokio::sync::RwLock;

/// Registry for dynamic tool management.
///
/// Wraps the static tools from McpRouter and adds runtime-registered tools.
#[derive(Clone)]
pub struct DynamicToolRegistry {
    inner: Arc<RwLockInner>,
}

struct RwLockInner {
    /// Runtime-registered tools
    dynamic_tools: RwLock<HashMap<String, DynamicTool>>,
    /// Notification sender for tools/list_changed
    notify_tx: broadcast::Sender<ToolsChangedNotification>,
}

impl DynamicToolRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self { ... }

    /// Register a new tool at runtime.
    ///
    /// Returns error if tool name already exists.
    pub async fn register(&self, tool: DynamicTool) -> Result<(), RegistryError> {
        let mut tools = self.inner.dynamic_tools.write().await;
        if tools.contains_key(&tool.name) {
            return Err(RegistryError::ToolExists(tool.name));
        }
        let name = tool.name.clone();
        tools.insert(name.clone(), tool);
        drop(tools);

        // Notify clients
        let _ = self.inner.notify_tx.send(ToolsChangedNotification {});

        Ok(())
    }

    /// Unregister a tool.
    pub async fn unregister(&self, name: &str) -> Result<DynamicTool, RegistryError> {
        let mut tools = self.inner.dynamic_tools.write().await;
        let tool = tools.remove(name)
            .ok_or_else(|| RegistryError::ToolNotFound(name.to_string()))?;
        drop(tools);

        let _ = self.inner.notify_tx.send(ToolsChangedNotification {});

        Ok(tool)
    }

    /// List all dynamic tools.
    pub async fn list(&self) -> Vec<ToolInfo> {
        let tools = self.inner.dynamic_tools.read().await;
        tools.values().map(|t| t.info()).collect()
    }

    /// Get a tool by name.
    pub async fn get(&self, name: &str) -> Option<DynamicTool> {
        let tools = self.inner.dynamic_tools.read().await;
        tools.get(name).cloned()
    }

    /// Subscribe to tools/list_changed notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<ToolsChangedNotification> {
        self.inner.notify_tx.subscribe()
    }

    // === Filtering by metadata (inspired by SEP-1300) ===

    /// List tools in a specific category.
    pub async fn list_by_category(&self, category: &str) -> Vec<ToolInfo> {
        let tools = self.inner.dynamic_tools.read().await;
        tools.values()
            .filter(|t| t.category.as_deref() == Some(category))
            .map(|t| t.info())
            .collect()
    }

    /// List tools with any of the specified tags.
    pub async fn list_by_tags(&self, tags: &[&str]) -> Vec<ToolInfo> {
        let tools = self.inner.dynamic_tools.read().await;
        tools.values()
            .filter(|t| t.tags.iter().any(|tag| tags.contains(&tag.as_str())))
            .map(|t| t.info())
            .collect()
    }

    /// List all categories in use.
    pub async fn categories(&self) -> Vec<String> {
        let tools = self.inner.dynamic_tools.read().await;
        let mut categories: Vec<_> = tools.values()
            .filter_map(|t| t.category.clone())
            .collect();
        categories.sort();
        categories.dedup();
        categories
    }

    // === Import/Export ===

    /// Export all tools as portable definitions.
    pub async fn export(&self) -> Vec<ToolDefinition> {
        let tools = self.inner.dynamic_tools.read().await;
        tools.values()
            .filter_map(|t| t.as_exportable())
            .collect()
    }

    /// Import tool definitions (composite tools only).
    /// Returns names of successfully imported tools.
    pub async fn import(
        &self,
        definitions: Vec<ToolDefinition>,
        created_by: &str,
    ) -> Result<Vec<String>, RegistryError> {
        let mut imported = Vec::new();
        for def in definitions {
            let tool = DynamicTool::from_definition(def, created_by)?;
            self.register(tool.clone()).await?;
            imported.push(tool.name);
        }
        Ok(imported)
    }
}

/// Portable tool definition for import/export.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    /// Steps for composite tools (only composite tools can be exported)
    pub steps: Vec<CompositeStepDef>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CompositeStepDef {
    pub tool: String,
    pub input_template: serde_json::Value,
    pub output_binding: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("tool already exists: {0}")]
    ToolExists(String),

    #[error("tool not found: {0}")]
    ToolNotFound(String),

    #[error("invalid tool definition: {0}")]
    InvalidDefinition(String),
}
```

### 2. DynamicTool

A tool with runtime-defined schema and handler.

```rust
/// A tool defined at runtime rather than compile time.
#[derive(Clone)]
pub struct DynamicTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,  // JSON Schema
    pub annotations: ToolAnnotations,
    handler: Arc<dyn DynamicHandler>,

    // Metadata (inspired by SEP-1300: Tool Filtering with Groups and Tags)
    /// Optional category for organization (e.g., "data-processing", "notifications")
    pub category: Option<String>,
    /// Tags for filtering and discovery (e.g., ["redis", "cache", "read"])
    pub tags: Vec<String>,
    /// Who created this tool (session ID or user identifier)
    pub created_by: String,
    /// When the tool was created
    pub created_at: std::time::SystemTime,
}

/// Handler for dynamic tools.
#[async_trait]
pub trait DynamicHandler: Send + Sync {
    /// Execute the tool with the given input.
    async fn call(
        &self,
        input: serde_json::Value,
        context: DynamicContext,
    ) -> Result<CallToolResult, ToolError>;
}

/// Context available to dynamic handlers.
pub struct DynamicContext {
    /// Access to call other tools
    pub tool_caller: ToolCaller,
    /// Progress reporting
    pub progress: Option<ProgressSender>,
    /// Session state
    pub session: Arc<SessionState>,
}

impl DynamicTool {
    pub fn builder(name: impl Into<String>, created_by: impl Into<String>) -> DynamicToolBuilder {
        DynamicToolBuilder::new(name, created_by)
    }

    pub fn info(&self) -> ToolInfo {
        ToolInfo {
            name: self.name.clone(),
            description: Some(self.description.clone()),
            input_schema: self.input_schema.clone(),
            annotations: Some(self.annotations.clone()),
        }
    }

    /// Export as a portable definition (only works for composite tools).
    pub fn as_exportable(&self) -> Option<ToolDefinition> {
        // Only composite handlers can be exported
        let composite = self.handler.as_any().downcast_ref::<CompositeHandler>()?;

        Some(ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            category: self.category.clone(),
            tags: self.tags.clone(),
            steps: composite.steps.iter().map(|s| CompositeStepDef {
                tool: s.tool.clone(),
                input_template: s.input_template.clone(),
                output_binding: s.output_binding.clone(),
            }).collect(),
        })
    }

    /// Create from a portable definition.
    pub fn from_definition(def: ToolDefinition, created_by: &str) -> Result<Self, RegistryError> {
        let steps: Vec<CompositeStep> = def.steps.into_iter()
            .map(|s| CompositeStep {
                tool: s.tool,
                input_template: s.input_template,
                output_binding: s.output_binding,
            })
            .collect();

        DynamicTool::builder(&def.name, created_by)
            .description(def.description)
            .category(def.category.unwrap_or_default())
            .tags(def.tags)
            .composite(steps)
            .build()
    }
}
```

### 3. DynamicToolBuilder

Ergonomic builder for dynamic tools.

```rust
pub struct DynamicToolBuilder {
    name: String,
    description: Option<String>,
    input_schema: Option<serde_json::Value>,
    annotations: ToolAnnotations,
    handler: Option<Arc<dyn DynamicHandler>>,
    // Metadata
    category: Option<String>,
    tags: Vec<String>,
    created_by: String,
}

impl DynamicToolBuilder {
    pub fn new(name: impl Into<String>, created_by: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            input_schema: None,
            annotations: ToolAnnotations::default(),
            handler: None,
            category: None,
            tags: Vec::new(),
            created_by: created_by.into(),
        }
    }

    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Set input schema from JSON value.
    pub fn input_schema(mut self, schema: serde_json::Value) -> Self {
        self.input_schema = Some(schema);
        self
    }

    /// Set input schema from a type (compile-time convenience).
    pub fn input_schema_from<T: JsonSchema>(mut self) -> Self {
        self.input_schema = Some(serde_json::to_value(T::json_schema()).unwrap());
        self
    }

    pub fn read_only(mut self) -> Self {
        self.annotations.read_only_hint = Some(true);
        self
    }

    pub fn destructive(mut self) -> Self {
        self.annotations.destructive_hint = Some(true);
        self
    }

    /// Categorize this tool for organization.
    pub fn category(mut self, category: impl Into<String>) -> Self {
        self.category = Some(category.into());
        self
    }

    /// Add tags for filtering and discovery.
    pub fn tags(mut self, tags: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Add a single tag.
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }

    /// Set handler from a closure.
    pub fn handler<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(serde_json::Value, DynamicContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<CallToolResult, ToolError>> + Send + 'static,
    {
        self.handler = Some(Arc::new(ClosureHandler(f)));
        self
    }

    /// Set handler for composite/workflow tools.
    pub fn composite(mut self, steps: Vec<CompositeStep>) -> Self {
        self.handler = Some(Arc::new(CompositeHandler::new(steps)));
        self
    }

    pub fn build(self) -> Result<DynamicTool, RegistryError> {
        Ok(DynamicTool {
            name: self.name,
            description: self.description.unwrap_or_default(),
            input_schema: self.input_schema.unwrap_or(json!({"type": "object"})),
            annotations: self.annotations,
            handler: self.handler.ok_or_else(||
                RegistryError::InvalidDefinition("handler required".into()))?,
            category: self.category,
            tags: self.tags,
            created_by: self.created_by,
            created_at: std::time::SystemTime::now(),
        })
    }
}
```

### 4. Router Integration

McpRouter gains dynamic tool support.

```rust
impl McpRouter {
    /// Enable dynamic tool registration.
    ///
    /// Returns a registry that can be used to add/remove tools at runtime.
    pub fn with_dynamic_tools(self) -> (Self, DynamicToolRegistry) {
        let registry = DynamicToolRegistry::new();
        let router = self.with_extension(registry.clone());
        (router, registry)
    }
}

// In the router's tool dispatch logic:
impl McpRouter {
    async fn handle_tools_call(&self, request: CallToolRequest) -> Result<CallToolResult, Error> {
        // First check static tools
        if let Some(tool) = self.static_tools.get(&request.name) {
            return tool.call(request.arguments).await;
        }

        // Then check dynamic registry
        if let Some(registry) = self.extensions.get::<DynamicToolRegistry>() {
            if let Some(tool) = registry.get(&request.name).await {
                let context = DynamicContext {
                    tool_caller: self.tool_caller(),
                    progress: request.progress,
                    session: self.session.clone(),
                };
                return tool.handler.call(request.arguments, context).await;
            }
        }

        Err(Error::tool_not_found(&request.name))
    }

    async fn handle_tools_list(&self) -> Vec<ToolInfo> {
        let mut tools: Vec<_> = self.static_tools.values()
            .map(|t| t.info())
            .collect();

        if let Some(registry) = self.extensions.get::<DynamicToolRegistry>() {
            tools.extend(registry.list().await);
        }

        tools
    }
}
```

## Composite Tool Executor

For workflow/sequence tools.

```rust
/// A step in a composite tool.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompositeStep {
    /// Tool to call
    pub tool: String,
    /// Arguments template with $variable substitution
    pub args: serde_json::Value,
    /// What to do on error
    #[serde(default)]
    pub on_error: OnError,
    /// Name to bind output to (for use in later steps)
    pub output_as: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    #[default]
    Stop,
    Continue,
    /// Return this value and continue
    Default(serde_json::Value),
}

/// Handler that executes a sequence of tool calls.
pub struct CompositeHandler {
    steps: Vec<CompositeStep>,
}

impl CompositeHandler {
    pub fn new(steps: Vec<CompositeStep>) -> Self {
        Self { steps }
    }
}

#[async_trait]
impl DynamicHandler for CompositeHandler {
    async fn call(
        &self,
        input: serde_json::Value,
        ctx: DynamicContext,
    ) -> Result<CallToolResult, ToolError> {
        let mut bindings = Bindings::new();
        bindings.insert("input", input);

        let mut last_result = None;

        for (i, step) in self.steps.iter().enumerate() {
            // Substitute variables in args
            let args = substitute(&step.args, &bindings)?;

            // Call the tool
            let result = ctx.tool_caller
                .call(&step.tool, args)
                .await;

            match result {
                Ok(output) => {
                    // Bind output for later steps
                    if let Some(name) = &step.output_as {
                        bindings.insert(name, output.to_json());
                    }
                    bindings.insert(&format!("step{}", i), output.to_json());
                    last_result = Some(output);
                }
                Err(e) => {
                    match &step.on_error {
                        OnError::Stop => return Err(e),
                        OnError::Continue => continue,
                        OnError::Default(val) => {
                            if let Some(name) = &step.output_as {
                                bindings.insert(name, val.clone());
                            }
                        }
                    }
                }
            }

            // Report progress
            if let Some(progress) = &ctx.progress {
                let pct = (i + 1) as f64 / self.steps.len() as f64;
                let _ = progress.send(pct, &format!("Completed step {}", i + 1)).await;
            }
        }

        last_result.ok_or_else(|| ToolError::new("no steps executed"))
    }
}

/// Variable substitution in JSON.
fn substitute(template: &serde_json::Value, bindings: &Bindings) -> Result<serde_json::Value, ToolError> {
    match template {
        serde_json::Value::String(s) if s.starts_with('$') => {
            // $input.field or $step0.result
            let path = &s[1..];
            bindings.get_path(path)
                .ok_or_else(|| ToolError::new(format!("unbound variable: {}", s)))
        }
        serde_json::Value::Object(map) => {
            let mut result = serde_json::Map::new();
            for (k, v) in map {
                result.insert(k.clone(), substitute(v, bindings)?);
            }
            Ok(serde_json::Value::Object(result))
        }
        serde_json::Value::Array(arr) => {
            let result: Result<Vec<_>, _> = arr.iter()
                .map(|v| substitute(v, bindings))
                .collect();
            Ok(serde_json::Value::Array(result?))
        }
        other => Ok(other.clone()),
    }
}
```

## Meta-Tools for Workflow Creation

Tools that create tools.

```rust
/// Input for the create_workflow meta-tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateWorkflowInput {
    /// Name for the new tool
    pub name: String,
    /// Description of what it does
    pub description: String,
    /// Input parameters the workflow accepts
    pub parameters: Vec<WorkflowParameter>,
    /// Steps to execute
    pub steps: Vec<CompositeStep>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowParameter {
    pub name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub param_type: String,  // "string", "number", "boolean"
    #[serde(default)]
    pub required: bool,
}

/// Build the meta-tools for workflow management.
pub fn workflow_tools(registry: DynamicToolRegistry) -> Vec<Tool> {
    vec![
        ToolBuilder::new("create_workflow")
            .description(
                "Create a new tool from a sequence of existing tool calls. \
                 The new tool will appear in tools/list and can be called like any other tool."
            )
            .handler_with_state(registry.clone(), create_workflow_handler)
            .build(),

        ToolBuilder::new("list_workflows")
            .description("List all dynamically created workflow tools")
            .handler_with_state(registry.clone(), list_workflows_handler)
            .read_only()
            .build(),

        ToolBuilder::new("delete_workflow")
            .description("Delete a dynamically created workflow tool")
            .handler_with_state(registry.clone(), delete_workflow_handler)
            .destructive()
            .build(),

        ToolBuilder::new("get_workflow_definition")
            .description("Get the step-by-step definition of a workflow tool")
            .handler_with_state(registry, get_workflow_handler)
            .read_only()
            .build(),
    ]
}

async fn create_workflow_handler(
    State(registry): State<DynamicToolRegistry>,
    input: CreateWorkflowInput,
) -> Result<CallToolResult, ToolError> {
    // Build input schema from parameters
    let input_schema = build_schema_from_params(&input.parameters);

    let tool = DynamicTool::builder(&input.name)
        .description(&input.description)
        .input_schema(input_schema)
        .composite(input.steps)
        .build()
        .map_err(|e| ToolError::new(e.to_string()))?;

    registry.register(tool).await
        .map_err(|e| ToolError::new(e.to_string()))?;

    Ok(CallToolResult::text(format!(
        "Created workflow '{}'. It is now available in tools/list.",
        input.name
    )))
}
```

## Usage Example

Agent creates a deploy workflow:

```json
{
  "tool": "create_workflow",
  "arguments": {
    "name": "deploy_to_staging",
    "description": "Push changes, wait for CI, deploy to staging, run smoke tests",
    "parameters": [
      { "name": "branch", "type": "string", "required": true, "description": "Branch to deploy" }
    ],
    "steps": [
      {
        "tool": "git_push",
        "args": { "branch": "$input.branch", "remote": "origin" },
        "output_as": "push"
      },
      {
        "tool": "wait_for_ci",
        "args": { "sha": "$push.sha" },
        "output_as": "ci"
      },
      {
        "tool": "deploy",
        "args": { "environment": "staging", "sha": "$ci.sha" },
        "output_as": "deploy"
      },
      {
        "tool": "run_smoke_tests",
        "args": { "url": "$deploy.url" }
      }
    ]
  }
}
```

Then uses it:

```json
{
  "tool": "deploy_to_staging",
  "arguments": { "branch": "feature/new-thing" }
}
```

## MCP Protocol Considerations

### tools/list_changed Notification

When tools are added/removed, notify connected clients:

```rust
// In the notification handling
impl McpRouter {
    fn setup_dynamic_notifications(&self) {
        if let Some(registry) = self.extensions.get::<DynamicToolRegistry>() {
            let notify_tx = self.notification_sender.clone();
            let mut rx = registry.subscribe();

            tokio::spawn(async move {
                while let Ok(_) = rx.recv().await {
                    let _ = notify_tx.send(Notification::ToolsListChanged).await;
                }
            });
        }
    }
}
```

### Persistence (Optional)

Workflows can be persisted across restarts:

```rust
impl DynamicToolRegistry {
    /// Load workflows from storage.
    pub async fn load_from(&self, path: &Path) -> Result<(), io::Error> {
        let data = tokio::fs::read_to_string(path).await?;
        let workflows: Vec<WorkflowDef> = serde_json::from_str(&data)?;
        for def in workflows {
            let tool = def.into_dynamic_tool()?;
            self.register(tool).await.ok(); // ignore duplicates
        }
        Ok(())
    }

    /// Save workflows to storage.
    pub async fn save_to(&self, path: &Path) -> Result<(), io::Error> {
        let tools = self.inner.dynamic_tools.read().await;
        let workflows: Vec<_> = tools.values()
            .filter_map(|t| t.as_workflow_def())
            .collect();
        let data = serde_json::to_string_pretty(&workflows)?;
        tokio::fs::write(path, data).await
    }
}
```

## Feature Flag

```toml
[features]
dynamic-tools = []
```

```rust
#[cfg(feature = "dynamic-tools")]
pub mod dynamic;
```

## Complete Example: Redis Workflow Server

A full implementation showing dynamic tools with a Redis MCP server.

### Running the Example

**Option 1: Fully managed with docker-wrapper (recommended)**

The example uses [docker-wrapper](https://crates.io/crates/docker-wrapper) to manage Redis
automatically from Rust:

```bash
# Just run it - Redis container starts automatically
cargo run --example redis-workflow-server --features dynamic-tools,docker-templates
```

**Option 2: External Redis**

```bash
# Start Redis manually
docker run -d --name redis-mcp-demo -p 6379:6379 redis:7-alpine

# Run the server with external Redis
REDIS_URL=redis://127.0.0.1:6379 cargo run --example redis-workflow-server --features dynamic-tools
```

### Dependencies

```toml
# In Cargo.toml

[features]
dynamic-tools = []
docker-templates = ["docker-wrapper/templates"]

[dependencies]
docker-wrapper = { version = "0.8", features = ["templates"], optional = true }
redis = { version = "0.27", features = ["tokio-comp"] }
```

### Setup and Configuration

```rust
use std::sync::Arc;
use tower_mcp::{
    McpRouter, ToolBuilder, CallToolResult, StdioTransport,
    dynamic::{DynamicToolRegistry, DynamicToolsConfig, CompositeToolFilter, CompositePermissionMode},
};
use redis::Client as RedisClient;

#[cfg(feature = "docker-templates")]
use docker_wrapper::{RedisTemplate, Template};

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    // === 0. Start Redis container (if using docker-wrapper) ===
    #[cfg(feature = "docker-templates")]
    let redis_template = {
        // docker-wrapper provides pre-configured container templates
        let template = RedisTemplate::new("redis-mcp-demo")
            .port(6379)
            .custom_image("redis", "7-alpine");

        eprintln!("Starting Redis container...");
        template.start().await?;
        eprintln!("Redis container started");
        template
    };

    // Connect to Redis
    let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let redis = RedisClient::open(redis_url)?;
    let redis = Arc::new(redis);
    // === 1. Configure dynamic tools ===
    let config = DynamicToolsConfig {
        // Allow up to 10 workflows per session
        max_tools_per_session: 10,

        // Workflows can have up to 20 steps
        max_steps: 20,

        // Only non-destructive tools can be composed
        // This means redis_del and redis_flushdb are blocked
        composite_tool_filter: CompositeToolFilter::NonDestructive,

        // Caller's permissions checked at each step
        composite_permission_mode: CompositePermissionMode::Caller,

        // Allow workflows to call other workflows (1 level deep)
        allow_composite_nesting: true,
        max_depth: 2,

        // Session-scoped only (no global workflows)
        allow_global_scope: false,

        ..Default::default()
    };

    // === 2. Create the dynamic registry ===
    let registry = DynamicToolRegistry::new(config);

    // === 3. Build Redis tools with proper annotations ===
    let redis_tools = build_redis_tools(redis.clone());

    // === 4. Build workflow management tools ===
    let workflow_tools = build_workflow_tools(registry.clone());

    // === 5. Assemble router ===
    let mut router = McpRouter::new()
        .server_info("redis-mcp", "1.0.0")
        .instructions(
            "Redis management server with dynamic workflow creation.\n\n\
             Use the workflow tools to create reusable command sequences:\n\
             - create_workflow: Define a new workflow from existing tools\n\
             - list_workflows: See available workflows\n\
             - delete_workflow: Remove a workflow\n\n\
             Workflows can only use non-destructive tools (no DEL, FLUSHDB)."
        )
        .with_dynamic_registry(registry);

    // Add all tools
    for tool in redis_tools {
        router = router.tool(tool);
    }
    for tool in workflow_tools {
        router = router.tool(tool);
    }

    // Run (blocks until Ctrl+C or connection closes)
    let result = StdioTransport::new(router).run().await;

    // === Cleanup: Stop Redis container ===
    #[cfg(feature = "docker-templates")]
    {
        eprintln!("Stopping Redis container...");
        redis_template.stop().await.ok();
        eprintln!("Redis container stopped");
    }

    result?;
    Ok(())
}
```

### Redis Tools with Annotations

```rust
fn build_redis_tools(redis: Arc<RedisClient>) -> Vec<tower_mcp::Tool> {
    vec![
        // === Read-only tools (safe for any workflow) ===

        ToolBuilder::new("redis_get")
            .description("Get the value of a key")
            .read_only()  // ← Marked safe
            .extractor_handler(|State(redis): State<RedisClient>, input: GetInput| async move {
                let value = redis.get(&input.key).await?;
                Ok(CallToolResult::text(value.unwrap_or_default()))
            })
            .build(),

        ToolBuilder::new("redis_keys")
            .description("Find keys matching a pattern")
            .read_only()  // ← Marked safe
            .extractor_handler(|State(redis): State<RedisClient>, input: KeysInput| async move {
                let keys = redis.keys(&input.pattern).await?;
                Ok(CallToolResult::json(&keys)?)
            })
            .build(),

        ToolBuilder::new("redis_info")
            .description("Get Redis server information")
            .read_only()  // ← Marked safe
            .extractor_handler(|State(redis): State<RedisClient>| async move {
                let info = redis.info().await?;
                Ok(CallToolResult::text(info))
            })
            .build(),

        ToolBuilder::new("redis_ttl")
            .description("Get time-to-live for a key in seconds")
            .read_only()  // ← Marked safe
            .extractor_handler(|State(redis): State<RedisClient>, input: KeyInput| async move {
                let ttl = redis.ttl(&input.key).await?;
                Ok(CallToolResult::text(ttl.to_string()))
            })
            .build(),

        // === Write tools (allowed in workflows, not destructive) ===

        ToolBuilder::new("redis_set")
            .description("Set a key to a value")
            // No annotation = not read_only, not destructive
            // NonDestructive filter will allow this
            .extractor_handler(|State(redis): State<RedisClient>, input: SetInput| async move {
                redis.set(&input.key, &input.value).await?;
                Ok(CallToolResult::text("OK"))
            })
            .build(),

        ToolBuilder::new("redis_expire")
            .description("Set a timeout on a key")
            .extractor_handler(|State(redis): State<RedisClient>, input: ExpireInput| async move {
                redis.expire(&input.key, input.seconds).await?;
                Ok(CallToolResult::text("OK"))
            })
            .build(),

        ToolBuilder::new("redis_incr")
            .description("Increment a key's integer value")
            .extractor_handler(|State(redis): State<RedisClient>, input: KeyInput| async move {
                let new_val = redis.incr(&input.key).await?;
                Ok(CallToolResult::text(new_val.to_string()))
            })
            .build(),

        // === Destructive tools (BLOCKED from workflows) ===

        ToolBuilder::new("redis_del")
            .description("Delete one or more keys")
            .destructive()  // ← Blocked by NonDestructive filter
            .extractor_handler(|State(redis): State<RedisClient>, input: DelInput| async move {
                let count = redis.del(&input.keys).await?;
                Ok(CallToolResult::text(format!("Deleted {} keys", count)))
            })
            .build(),

        ToolBuilder::new("redis_flushdb")
            .description("Delete all keys in the current database")
            .destructive()  // ← Blocked by NonDestructive filter
            .extractor_handler(|State(redis): State<RedisClient>| async move {
                redis.flushdb().await?;
                Ok(CallToolResult::text("OK"))
            })
            .build(),
    ]
}
```

### Workflow Management Tools

```rust
fn build_workflow_tools(registry: DynamicToolRegistry) -> Vec<tower_mcp::Tool> {
    vec![
        ToolBuilder::new("create_workflow")
            .description(
                "Create a reusable workflow from a sequence of tool calls. \
                 Workflows can only use non-destructive tools."
            )
            .extractor_handler({
                let registry = registry.clone();
                move |input: CreateWorkflowInput| {
                    let registry = registry.clone();
                    async move {
                        // Build input schema from parameters
                        let schema = build_schema(&input.parameters);

                        // Create the composite tool
                        let tool = DynamicTool::builder(&input.name)
                            .description(&input.description)
                            .input_schema(schema)
                            .composite(input.steps)
                            .build()?;

                        // Register it
                        registry.register(tool, ToolScope::Session).await?;

                        Ok(CallToolResult::text(format!(
                            "Created workflow '{}' with {} steps. \
                             It's now available as a tool.",
                            input.name, input.steps.len()
                        )))
                    }
                }
            })
            .build(),

        ToolBuilder::new("list_workflows")
            .description("List all workflows created in this session")
            .read_only()
            .extractor_handler({
                let registry = registry.clone();
                move || {
                    let registry = registry.clone();
                    async move {
                        let workflows = registry.list_composites().await;
                        Ok(CallToolResult::json(&workflows)?)
                    }
                }
            })
            .build(),

        ToolBuilder::new("delete_workflow")
            .description("Delete a workflow")
            .extractor_handler({
                let registry = registry.clone();
                move |input: DeleteWorkflowInput| {
                    let registry = registry.clone();
                    async move {
                        registry.unregister(&input.name).await?;
                        Ok(CallToolResult::text(format!("Deleted workflow '{}'", input.name)))
                    }
                }
            })
            .build(),

        ToolBuilder::new("describe_workflow")
            .description("Show the steps in a workflow")
            .read_only()
            .extractor_handler({
                let registry = registry.clone();
                move |input: DescribeWorkflowInput| {
                    let registry = registry.clone();
                    async move {
                        let workflow = registry.get(&input.name).await
                            .ok_or_else(|| ToolError::not_found(&input.name))?;
                        Ok(CallToolResult::json(&workflow.definition())?)
                    }
                }
            })
            .build(),

        // === Category/Tag filtering (inspired by SEP-1300) ===

        ToolBuilder::new("list_workflow_categories")
            .description("List all workflow categories in use")
            .read_only()
            .extractor_handler({
                let registry = registry.clone();
                move || {
                    let registry = registry.clone();
                    async move {
                        let categories = registry.categories().await;
                        Ok(CallToolResult::json(&categories)?)
                    }
                }
            })
            .build(),

        ToolBuilder::new("filter_workflows_by_tag")
            .description("List workflows that have any of the specified tags")
            .read_only()
            .extractor_handler({
                let registry = registry.clone();
                move |input: FilterByTagInput| {
                    let registry = registry.clone();
                    async move {
                        let tags: Vec<&str> = input.tags.iter().map(|s| s.as_str()).collect();
                        let workflows = registry.list_by_tags(&tags).await;
                        Ok(CallToolResult::json(&workflows)?)
                    }
                }
            })
            .build(),

        // === Import/Export ===

        ToolBuilder::new("export_workflows")
            .description("Export all workflows as portable definitions (for backup or sharing)")
            .read_only()
            .extractor_handler({
                let registry = registry.clone();
                move || {
                    let registry = registry.clone();
                    async move {
                        let definitions = registry.export().await;
                        Ok(CallToolResult::json(&definitions)?)
                    }
                }
            })
            .build(),

        ToolBuilder::new("import_workflows")
            .description("Import workflow definitions (from export_workflows output)")
            .extractor_handler({
                let registry = registry.clone();
                move |input: ImportWorkflowsInput, ctx: Context| {
                    let registry = registry.clone();
                    async move {
                        let created_by = ctx.session_id().unwrap_or("unknown");
                        let imported = registry.import(input.definitions, created_by).await?;
                        Ok(CallToolResult::text(format!(
                            "Imported {} workflows: {}",
                            imported.len(),
                            imported.join(", ")
                        )))
                    }
                }
            })
            .build(),
    ]
}
```

### Input Types

```rust
#[derive(Debug, Deserialize, JsonSchema)]
struct CreateWorkflowInput {
    /// Name for the new workflow
    name: String,
    /// What this workflow does
    description: String,
    /// Optional category for organization (e.g., "cache-management", "monitoring")
    #[serde(default)]
    category: Option<String>,
    /// Tags for filtering and discovery
    #[serde(default)]
    tags: Vec<String>,
    /// Parameters the workflow accepts
    #[serde(default)]
    parameters: Vec<WorkflowParam>,
    /// Steps to execute in order
    steps: Vec<CompositeStep>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkflowParam {
    name: String,
    #[serde(rename = "type")]
    param_type: String,
    #[serde(default)]
    required: bool,
    description: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CompositeStep {
    /// Tool to call
    tool: String,
    /// Arguments (use $input.field for parameters, $stepN.field for previous results)
    args: serde_json::Value,
    /// Name to reference this step's output
    #[serde(default)]
    output_as: Option<String>,
    /// What to do on error: "stop" (default), "continue", or {"default": value}
    #[serde(default)]
    on_error: OnError,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DeleteWorkflowInput {
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DescribeWorkflowInput {
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FilterByTagInput {
    /// Tags to filter by (workflows matching any tag are returned)
    tags: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ImportWorkflowsInput {
    /// Workflow definitions (from export_workflows output)
    definitions: Vec<ToolDefinition>,
}
```

### Example Session

```
User: "I keep checking the same set of keys for a cache warmup verification.
       Can you create a workflow for that?"

Agent: I'll create a workflow that checks multiple cache keys and reports their status.

[Calls create_workflow with:]
{
  "name": "verify_cache_warmup",
  "description": "Check if cache keys exist and have expected TTLs",
  "parameters": [
    {"name": "prefix", "type": "string", "required": true, "description": "Key prefix to check"}
  ],
  "steps": [
    {
      "tool": "redis_keys",
      "args": {"pattern": "$input.prefix:*"},
      "output_as": "keys"
    },
    {
      "tool": "redis_get",
      "args": {"key": "$input.prefix:users"},
      "output_as": "users_cache"
    },
    {
      "tool": "redis_ttl",
      "args": {"key": "$input.prefix:users"},
      "output_as": "users_ttl"
    },
    {
      "tool": "redis_get",
      "args": {"key": "$input.prefix:products"},
      "output_as": "products_cache"
    },
    {
      "tool": "redis_ttl",
      "args": {"key": "$input.prefix:products"},
      "output_as": "products_ttl"
    }
  ]
}

Response: Created workflow 'verify_cache_warmup' with 5 steps. It's now available as a tool.

User: "Great, now check the production cache"

Agent: [Calls verify_cache_warmup with {"prefix": "prod"}]

Response:
{
  "keys": ["prod:users", "prod:products", "prod:sessions"],
  "users_cache": "{...cached data...}",
  "users_ttl": 3200,
  "products_cache": "{...cached data...}",
  "products_ttl": 3198
}

Agent: The production cache is warmed up. Found 3 keys with the prod: prefix.
       Both users and products caches are populated with ~53 minutes TTL remaining.
```

### What the Config Prevented

If the user tried to create a workflow with `redis_del`:

```
User: "Create a workflow that clears and rebuilds the cache"

Agent: [Attempts create_workflow with redis_del step]

Error: Tool 'redis_del' is marked as destructive.
       Destructive tools cannot be used in composites with this filter.

Agent: I can't include redis_del in a workflow because it's marked as destructive.
       You'll need to call redis_del directly, then I can create a workflow for
       the rebuild steps.
```

### Permission Mode in Action

If using `CompositePermissionMode::Caller` (default) and a user without `redis_set` permission:

```
User (read-only role): "Run the cache_rebuild workflow"

[Workflow tries to execute redis_set step]

Error: Caller lacks permission for tool 'redis_set' in composite step.
       The composite can only use tools the caller can access directly.

Agent: You don't have permission to run this workflow because it includes
       redis_set, which requires write access. Contact your administrator
       for write permissions, or use the read-only verify_cache_warmup workflow.
```

## Configuration

Philosophy: **Safe by default, unlock with intention.** All limits are configurable
but defaults are conservative. Operators must explicitly opt into more permissive
settings.

### CompositeToolFilter

Filter which tools can be used in composite workflows. Leverages existing tool annotations.

```rust
/// Filter for which tools composites are allowed to call.
///
/// This enables annotation-driven permissions: mark your tools as `read_only()`,
/// `destructive()`, etc., and the filter automatically enforces what can be composed.
#[derive(Clone)]
pub enum CompositeToolFilter {
    /// Only tools with read_only_hint = true (safest, default)
    ReadOnlyOnly,

    /// Only tools without destructive_hint = true
    NonDestructive,

    /// Explicit allowlist of tool names
    AllowList(HashSet<String>),

    /// All static tools (no filtering)
    AllStatic,

    /// Combine filters - all must pass
    All(Vec<CompositeToolFilter>),

    /// Combine filters - any must pass
    Any(Vec<CompositeToolFilter>),

    /// Custom predicate
    Custom(Arc<dyn Fn(&ToolInfo) -> bool + Send + Sync>),
}

impl Default for CompositeToolFilter {
    fn default() -> Self {
        Self::ReadOnlyOnly
    }
}

impl CompositeToolFilter {
    /// Check if a tool passes this filter.
    pub fn allows(&self, tool: &ToolInfo) -> bool {
        match self {
            Self::ReadOnlyOnly => tool.annotations
                .as_ref()
                .and_then(|a| a.read_only_hint)
                .unwrap_or(false),
            Self::NonDestructive => !tool.annotations
                .as_ref()
                .and_then(|a| a.destructive_hint)
                .unwrap_or(false),
            Self::AllowList(set) => set.contains(&tool.name),
            Self::AllStatic => true,
            Self::All(filters) => filters.iter().all(|f| f.allows(tool)),
            Self::Any(filters) => filters.iter().any(|f| f.allows(tool)),
            Self::Custom(f) => f(tool),
        }
    }

    /// Human-readable reason why a tool was rejected.
    pub fn rejection_reason(&self, tool: &ToolInfo) -> String {
        match self {
            Self::ReadOnlyOnly => format!(
                "Tool '{}' is not marked as read_only. \
                 Add .read_only() to the tool or change composite_tool_filter.",
                tool.name
            ),
            Self::NonDestructive => format!(
                "Tool '{}' is marked as destructive. \
                 Destructive tools cannot be used in composites with this filter.",
                tool.name
            ),
            Self::AllowList(set) => format!(
                "Tool '{}' is not in the allowlist. Allowed: {:?}",
                tool.name, set
            ),
            _ => format!("Tool '{}' rejected by filter", tool.name),
        }
    }
}
```

### CompositePermissionMode

Controls how permissions are checked when a composite tool executes.

```rust
/// How to check permissions for composite step execution.
///
/// This prevents privilege escalation: even if a composite was created by
/// an admin, a regular user running it shouldn't gain admin powers.
#[derive(Clone, Debug, Default)]
pub enum CompositePermissionMode {
    /// Each step is checked against the CALLER's permissions (default, safest).
    /// The composite can only do what the caller could do directly.
    #[default]
    Caller,

    /// Steps are checked against the CREATOR's permissions.
    /// Like setuid - composite runs with creator's permissions.
    /// Use only for trusted, audited workflows.
    Creator,

    /// No step-level permission checks.
    /// Only the composite tool itself is checked.
    /// Most permissive, use with caution.
    None,
}
```

### DynamicToolsConfig

```rust
/// Configuration for dynamic tool registration and execution.
///
/// All defaults are conservative. Increase limits only after understanding
/// the implications for your deployment.
#[derive(Clone, Debug)]
pub struct DynamicToolsConfig {
    // === Creation Limits ===

    /// Maximum dynamic tools per session. Prevents resource exhaustion.
    /// Default: 5
    pub max_tools_per_session: usize,

    /// Maximum dynamic tools globally. Prevents unbounded growth.
    /// Default: 20
    pub max_tools_global: usize,

    /// Rate limit: tool creations per minute per session.
    /// Default: 3
    pub creation_rate_limit: usize,

    // === Execution Limits ===

    /// Maximum steps in a composite tool execution.
    /// Prevents runaway workflows.
    /// Default: 10
    pub max_steps: usize,

    /// Maximum nesting depth for composites calling composites.
    /// Prevents infinite recursion.
    /// Default: 2
    pub max_depth: usize,

    /// Maximum total execution time for a composite.
    /// Default: 60 seconds
    pub max_execution_time: Duration,

    /// Filter for which tools composites can reference.
    /// Default: ReadOnlyOnly (only tools with read_only_hint = true)
    pub composite_tool_filter: CompositeToolFilter,

    /// Whether composites can call other composites.
    /// Default: false
    pub allow_composite_nesting: bool,

    /// How to check permissions when executing composite steps.
    /// Default: Caller (each step checked against caller's permissions)
    pub composite_permission_mode: CompositePermissionMode,

    // === Scope & Lifecycle ===

    /// Whether to allow global (cross-session) tools.
    /// Default: false (session-scoped only)
    pub allow_global_scope: bool,

    /// TTL for session-scoped tools after last use.
    /// None = expire with session only.
    /// Default: None
    pub session_tool_ttl: Option<Duration>,

    /// TTL for global tools after last use.
    /// Only applies if allow_global_scope is true.
    /// Default: 1 hour
    pub global_tool_ttl: Duration,

    /// How often to run cleanup of expired tools.
    /// Default: 5 minutes
    pub cleanup_interval: Duration,

    // === Versioning ===

    /// Whether to snapshot underlying tool schemas at creation.
    /// If true, composite fails if underlying tool changes.
    /// Default: false (best-effort execution)
    pub strict_schema_checking: bool,

    // === Persistence ===

    /// Path to persist global tools across restarts.
    /// None = no persistence (tools lost on restart).
    /// Default: None
    pub persistence_path: Option<PathBuf>,
}

impl Default for DynamicToolsConfig {
    fn default() -> Self {
        Self {
            // Creation: very conservative
            max_tools_per_session: 5,
            max_tools_global: 20,
            creation_rate_limit: 3,

            // Execution: tight bounds
            max_steps: 10,
            max_depth: 2,
            max_execution_time: Duration::from_secs(60),
            composite_tool_filter: CompositeToolFilter::ReadOnlyOnly,
            allow_composite_nesting: false,
            composite_permission_mode: CompositePermissionMode::Caller,

            // Scope: session only by default
            allow_global_scope: false,
            session_tool_ttl: None,
            global_tool_ttl: Duration::from_secs(3600),
            cleanup_interval: Duration::from_secs(300),

            // Versioning: permissive
            strict_schema_checking: false,

            // Persistence: off
            persistence_path: None,
        }
    }
}
```

### Configuration Guide

#### When to increase `max_tools_per_session`

Increase if agents legitimately need many task-specific workflows. Consider:
- What's the typical session duration?
- Are tools being created and abandoned, or reused?
- Is there a cleanup strategy?

**Risk**: Memory exhaustion, tool list pollution.

#### When to increase `max_steps`

Increase for complex multi-stage workflows. Consider:
- Can the workflow be broken into smaller composites?
- Is progress reporting working so users see what's happening?
- What's the failure mode if a step fails late in the sequence?

**Risk**: Long-running operations, unclear failure states.

#### When to change `composite_tool_filter`

The default `ReadOnlyOnly` means composites can only chain tools marked with `.read_only()`.
This encourages proper annotation of your tools.

```rust
// Default: only read-only tools (safest)
config.composite_tool_filter = CompositeToolFilter::ReadOnlyOnly;

// Allow non-destructive writes (e.g., create but not delete)
config.composite_tool_filter = CompositeToolFilter::NonDestructive;

// Explicit allowlist
config.composite_tool_filter = CompositeToolFilter::AllowList(hashset![
    "git_status", "git_log", "git_diff",  // reads
    "git_add", "git_commit",               // safe writes
    // NOT: "git_push", "git_reset"        // dangerous
]);

// Combine: must be in list AND non-destructive
config.composite_tool_filter = CompositeToolFilter::All(vec![
    CompositeToolFilter::AllowList(hashset!["redis_get", "redis_set", "redis_del"]),
    CompositeToolFilter::NonDestructive,
]);

// Custom logic for redis: reads always OK, writes only for non-production keys
config.composite_tool_filter = CompositeToolFilter::Custom(Arc::new(|tool| {
    if tool.name.starts_with("redis_get") { return true; }
    if tool.name.starts_with("redis_set") {
        // Would need to check args at runtime, this is creation-time only
        return true;
    }
    false
}));
```

**Risk with ReadOnlyOnly**: Tools without `.read_only()` annotation can't be composed.
**Risk with AllStatic**: Any tool can be chained, including destructive ones.

#### When to change `composite_permission_mode`

Controls privilege escalation. Default is `Caller` (safest).

```rust
// Default: composite can only do what caller could do directly
config.composite_permission_mode = CompositePermissionMode::Caller;

// Creator mode: composite runs with creator's permissions (like setuid)
// Use only for trusted, audited workflows
config.composite_permission_mode = CompositePermissionMode::Creator;

// No checks: only the composite itself is permission-checked
// Most dangerous, use with caution
config.composite_permission_mode = CompositePermissionMode::None;
```

**Scenario**: Admin creates composite "cleanup" that calls "delete_database".

| Mode | Regular user calls "cleanup" | Result |
|------|------------------------------|--------|
| Caller | Fails - user can't call delete_database | ✓ Safe |
| Creator | Succeeds - runs as admin | ⚠️ Privilege escalation |
| None | Succeeds - no step checks | ⚠️ Privilege escalation |

**Recommendation**: Use `Caller` unless you have a specific need for elevated workflows
AND you have audit logging to track who created what.

#### When to enable `allow_composite_nesting`

Enable for meta-workflows (workflows that orchestrate workflows). Consider:
- Is `max_depth` set appropriately?
- Can you trace execution through nested composites?
- What happens if an inner composite changes?

**Risk**: Harder to debug, potential for deep recursion.

#### When to enable `allow_global_scope`

Enable for shared team workflows. Consider:
- Who can create global tools?
- Who can delete them?
- Is there audit logging?
- How do you handle conflicting tool names?

**Risk**: Cross-session interference, permission confusion.

#### When to enable `strict_schema_checking`

Enable for production workflows where reliability matters:

```rust
config.strict_schema_checking = true;
```

With this enabled:
- Tool schemas are captured at workflow creation time
- If underlying tool schema changes, workflow fails loudly
- Forces explicit workflow recreation after tool updates

**Risk if false**: Workflow may break silently if underlying tools change.
**Risk if true**: More maintenance burden when tools evolve.

### Example Configurations

#### Local Development (Permissive)

```rust
let config = DynamicToolsConfig {
    max_tools_per_session: 50,
    max_steps: 100,
    max_depth: 5,
    max_execution_time: Duration::from_secs(300),
    composite_tool_filter: CompositeToolFilter::AllStatic,  // any tool
    allow_composite_nesting: true,
    allow_global_scope: true,
    composite_permission_mode: CompositePermissionMode::None,
    global_tool_ttl: Duration::from_secs(86400),  // 24h
    ..Default::default()
};
```

#### Production API Server (Conservative)

```rust
let config = DynamicToolsConfig {
    max_tools_per_session: 3,
    max_steps: 5,
    max_depth: 1,  // no nesting
    max_execution_time: Duration::from_secs(30),
    composite_tool_filter: CompositeToolFilter::AllowList(
        hashset!["safe_tool_1", "safe_tool_2"]
    ),
    allow_composite_nesting: false,
    allow_global_scope: false,
    composite_permission_mode: CompositePermissionMode::Caller,
    strict_schema_checking: true,
    ..Default::default()
};
```

#### Multi-Tenant Platform (Isolated)

```rust
let config = DynamicToolsConfig {
    max_tools_per_session: 10,
    max_tools_global: 0,  // no global tools
    allow_global_scope: false,
    composite_tool_filter: CompositeToolFilter::ReadOnlyOnly,  // only reads
    composite_permission_mode: CompositePermissionMode::Caller,
    session_tool_ttl: Some(Duration::from_secs(3600)),  // 1h inactivity
    ..Default::default()
};
```

#### Redis MCP Server (Annotation-Driven)

```rust
// Tools are annotated:
// - redis_get, redis_keys, redis_info → .read_only()
// - redis_set, redis_hset → (no annotation, not destructive)
// - redis_del, redis_flushdb → .destructive()

let config = DynamicToolsConfig {
    // Allow reads and non-destructive writes in composites
    composite_tool_filter: CompositeToolFilter::NonDestructive,
    // Caller permissions at runtime
    composite_permission_mode: CompositePermissionMode::Caller,
    ..Default::default()
};

// Result: composites can chain get/set/keys but NOT del/flushdb
```

### Runtime Limit Enforcement

```rust
impl DynamicToolRegistry {
    async fn register(&self, tool: DynamicTool, scope: ToolScope) -> Result<(), RegistryError> {
        let config = &self.config;

        // Check session limit
        let session_count = self.count_session_tools(&scope).await;
        if session_count >= config.max_tools_per_session {
            return Err(RegistryError::SessionLimitReached {
                current: session_count,
                max: config.max_tools_per_session,
            });
        }

        // Check global limit
        let global_count = self.count_all_tools().await;
        if global_count >= config.max_tools_global {
            return Err(RegistryError::GlobalLimitReached {
                current: global_count,
                max: config.max_tools_global,
            });
        }

        // Check rate limit
        if !self.rate_limiter.check(&scope.session_id()) {
            return Err(RegistryError::RateLimited {
                retry_after: self.rate_limiter.retry_after(&scope.session_id()),
            });
        }

        // Check global scope permission
        if matches!(scope, ToolScope::Global { .. }) && !config.allow_global_scope {
            return Err(RegistryError::GlobalScopeDisabled);
        }

        // Validate composite if applicable
        if let Some(composite) = tool.as_composite() {
            self.validate_composite(composite, config)?;
        }

        // All checks passed
        self.inner.insert(tool, scope).await
    }

    fn validate_composite(&self, composite: &CompositeHandler, config: &DynamicToolsConfig) -> Result<(), RegistryError> {
        // Check step count
        if composite.steps.len() > config.max_steps {
            return Err(RegistryError::TooManySteps {
                count: composite.steps.len(),
                max: config.max_steps,
            });
        }

        // Check tool filter (annotation-based or allowlist)
        for step in &composite.steps {
            let tool_info = self.get_tool_info(&step.tool)
                .ok_or_else(|| RegistryError::ToolNotFound(step.tool.clone()))?;

            if !config.composite_tool_filter.allows(&tool_info) {
                return Err(RegistryError::ToolNotAllowedInComposite {
                    tool: step.tool.clone(),
                    reason: config.composite_tool_filter.rejection_reason(&tool_info),
                });
            }

            // Check nesting
            if self.is_composite(&step.tool) && !config.allow_composite_nesting {
                return Err(RegistryError::CompositeNestingDisabled);
            }
        }

        Ok(())
    }
}

// Runtime permission checking during execution
impl CompositeHandler {
    async fn call(&self, input: Value, ctx: DynamicContext) -> Result<CallToolResult, ToolError> {
        for step in &self.steps {
            // Check permissions based on mode
            match ctx.config.composite_permission_mode {
                CompositePermissionMode::Caller => {
                    if !ctx.caller_can_call(&step.tool).await? {
                        return Err(ToolError::permission_denied(format!(
                            "Caller lacks permission for tool '{}' in composite step. \
                             The composite can only use tools the caller can access directly.",
                            step.tool
                        )));
                    }
                }
                CompositePermissionMode::Creator => {
                    if !ctx.creator_can_call(&step.tool).await? {
                        return Err(ToolError::permission_denied(format!(
                            "Composite creator lacks permission for tool '{}'",
                            step.tool
                        )));
                    }
                }
                CompositePermissionMode::None => {
                    // No step-level checks
                }
            }

            // Execute step...
        }
        // ...
    }
}
```

### Error Messages

Clear, actionable error messages:

```rust
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(
        "Session tool limit reached ({current}/{max}). \
         Delete unused tools with delete_workflow or increase max_tools_per_session."
    )]
    SessionLimitReached { current: usize, max: usize },

    #[error(
        "Global tool limit reached ({current}/{max}). \
         This affects all sessions. Contact your administrator."
    )]
    GlobalLimitReached { current: usize, max: usize },

    #[error(
        "Rate limited. Try again in {retry_after:?}. \
         Creation rate is limited to prevent abuse."
    )]
    RateLimited { retry_after: Duration },

    #[error(
        "Global scope is disabled. Tools are session-scoped only. \
         To enable global tools, set allow_global_scope = true in config."
    )]
    GlobalScopeDisabled,

    #[error(
        "Workflow has {count} steps but maximum is {max}. \
         Break into smaller workflows or increase max_steps."
    )]
    TooManySteps { count: usize, max: usize },

    #[error("{reason}")]
    ToolNotAllowedInComposite { tool: String, reason: String },

    #[error("Tool not found: {0}")]
    ToolNotFound(String),

    #[error(
        "Permission denied: {0}. \
         With CompositePermissionMode::Caller, composites can only use tools \
         the caller can access directly."
    )]
    PermissionDenied(String),

    #[error(
        "Composite nesting is disabled. Workflows cannot call other workflows. \
         To enable, set allow_composite_nesting = true in config."
    )]
    CompositeNestingDisabled,

    #[error(
        "Tool schema mismatch for '{tool}'. Schema changed since workflow creation. \
         Recreate the workflow to pick up the new schema."
    )]
    SchemaMismatch { tool: String },
}
```

---

## Prior Art & References

This design draws on existing implementations and discussions in the MCP ecosystem.

### Official SDK Support

Dynamic tool registration is a first-class feature in the official MCP SDKs:

| SDK | Registration Method | Notification |
|-----|---------------------|--------------|
| Python | `mcp.add_tool()`, `mcp.remove_tool()` | `notifications/tools/list_changed` |
| TypeScript | `server.addTool()` | Automatic `listChanged` emission |
| Spring AI | `McpSyncServer.addTool()`, `removeTool()` | Built-in notification support |

The existence of `tools/list_changed` in the MCP spec explicitly anticipates runtime tool list mutations.

**References:**
- [Python SDK - Tool Management](https://github.com/modelcontextprotocol/python-sdk)
- [TypeScript SDK - Dynamic Tools](https://github.com/modelcontextprotocol/typescript-sdk)
- [Spring AI - Dynamic Tool Updates](https://spring.io/blog/2025/05/04/spring-ai-dynamic-tool-updates-with-mcp/)

### Existing Implementations

Several projects have implemented dynamic tool creation:

#### AI Meta MCP Server
AI-powered dynamic tool creation with sandboxed execution (JavaScript, Python, Shell).
Features human approval workflows and persistence between sessions.
- https://mcpmarket.com/server/ai-meta

#### DIY Tools MCP
User-facing tool creation via JSON definitions. Supports multiple languages
(Python, JavaScript, Bash, Ruby, TypeScript) with automatic validation.
- https://github.com/hesreallyhim/diy-tools-mcp

#### dynamic-mcp-server
Framework supporting both static and dynamic tool registration with SSE transport
for testing dynamic creation/management.
- https://github.com/scitara-cto/dynamic-mcp-server

#### lazy-mcp
MCP proxy implementing the meta-tool pattern with on-demand loading.
Reduces context window consumption through hierarchical exploration.
- https://github.com/voicetreelab/lazy-mcp

### Spec Discussions

Active discussions in the MCP repository informed this design:

#### Discussion #643: Dynamic Tool Exposure Based on Server State
Proposes using `ToolListChangedNotification` for multi-stage workflows where
different phases require different tool sets (research → summarization → integration).
- https://github.com/modelcontextprotocol/modelcontextprotocol/discussions/643

#### Discussion #532: Hierarchical Tool Management
Addresses context window saturation with large tool lists. Proposes category-based
discovery and lazy loading via `tools/categories` and `tools/discover` methods.
- https://github.com/orgs/modelcontextprotocol/discussions/532

#### SEP-1300: Tool Filtering with Groups and Tags
Proposes client-side and server-side filtering approaches with groups (first-class
objects for tool collections) and tags (lightweight labels for categorization).
- https://github.com/modelcontextprotocol/modelcontextprotocol/issues/1300

#### Issue #682: Dynamic Tool Registration Timing
Documents that dynamically registered tools aren't available mid-chain in some clients.
Our design ensures workflows are immediately available after creation.
- https://github.com/modelcontextprotocol/typescript-sdk/issues/682

### Design Patterns

#### The Meta-Tool Pattern
Uses discovery + execution tools instead of loading all tool schemas upfront.
Reduces token overhead by 85-95% for large tool sets. Our `list_workflows` +
`describe_workflow` + workflow execution follows this pattern.
- https://blog.synapticlabs.ai/bounded-context-packs-meta-tool-pattern

#### Tool Composition Patterns
Documents that repeated tool sequences indicate composition is needed.
Recommends Unix pipe principle: consistent response shapes, batch support.
- https://blog.arcade.dev/mcp-tool-patterns

#### Code Execution for Tool Composition
Anthropic's engineering blog discusses agents writing code to compose tools
rather than calling tools directly—a pattern for efficient multi-tool workflows.
- https://www.anthropic.com/engineering/code-execution-with-mcp

### Comparison with Prior Art

| Feature | AI Meta | DIY Tools | lazy-mcp | **This RFC** |
|---------|---------|-----------|----------|--------------|
| Arbitrary code execution | ✓ (sandboxed) | ✓ | ✗ | ✗ |
| Composite tool sequences | ✗ | ✗ | ✗ | ✓ |
| Variable substitution | ✗ | ✗ | ✗ | ✓ |
| Annotation-based filtering | ✗ | ✗ | ✗ | ✓ |
| Permission modes | ✓ (approval) | ✗ | ✗ | ✓ (Caller/Creator/None) |
| Session scoping | ✓ | ✗ | ✗ | ✓ |
| Nesting control | ✗ | ✗ | ✓ | ✓ |
| Schema validation | ✗ | ✓ | ✗ | ✓ (optional strict mode) |

### Ideas Incorporated from Prior Art

1. **Meta-tool pattern** (lazy-mcp): Our `list_workflows`, `describe_workflow` provide
   progressive disclosure without loading all workflow definitions upfront.

2. **Immediate availability** (Issue #682): Workflows are usable immediately after
   creation—no waiting for next request cycle.

3. **Approval workflows** (AI Meta): Our `CompositePermissionMode::Caller` ensures
   runtime permission checking at each step.

4. **Hierarchical organization** (Discussion #532): Workflow categories can help
   manage large numbers of workflows (future enhancement).

5. **Annotation-based filtering** (SEP-1300): `CompositeToolFilter::ReadOnlyOnly` and
   `NonDestructive` leverage existing tool annotations.

### Future Considerations

Based on prior art, potential future enhancements:

- **Workflow categories/tags**: Organization for large workflow collections (per SEP-1300)
- **Import/export**: Share workflow definitions between sessions or users
- **Template workflows**: Server-defined starter workflows that users can customize
- **Semantic filtering**: Embedding-based tool selection (per Portkey's mcp-tool-filter)
- **Workflow versioning**: Explicit version tracking beyond schema mismatch detection
