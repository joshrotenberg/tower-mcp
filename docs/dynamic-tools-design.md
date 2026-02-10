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
