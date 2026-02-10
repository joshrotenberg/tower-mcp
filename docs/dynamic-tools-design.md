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
    pub fn builder(name: impl Into<String>) -> DynamicToolBuilder {
        DynamicToolBuilder::new(name)
    }

    pub fn info(&self) -> ToolInfo {
        ToolInfo {
            name: self.name.clone(),
            description: Some(self.description.clone()),
            input_schema: self.input_schema.clone(),
            annotations: Some(self.annotations.clone()),
        }
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
}

impl DynamicToolBuilder {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            input_schema: None,
            annotations: ToolAnnotations::default(),
            handler: None,
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

## Open Questions

1. **Security**: Should there be limits on what dynamic tools can do?
   - Rate limiting on tool creation?
   - Sandboxing for composite execution?
   - Approval workflow before tool becomes available?

2. **Scope**: Per-session dynamic tools vs global?
   - Session-scoped: tool only visible to the session that created it
   - Global: visible to all sessions (requires auth consideration)

3. **Cleanup**: Auto-expire unused dynamic tools?
   - TTL on dynamic tools
   - Usage tracking

4. **Versioning**: What if a workflow references a tool that changes?
   - Pin to tool version?
   - Fail if underlying tool changes?
