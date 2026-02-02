# Router Composition Design for tower-mcp

RFC for issue #280 - merge and nest operations for McpRouter.

## Motivation

Large MCP servers benefit from modular organization:

```rust
// Instead of one giant router definition...
let router = McpRouter::new()
    .tool(user_create).tool(user_delete).tool(user_list)
    .tool(post_create).tool(post_delete).tool(post_list)
    .tool(comment_create).tool(comment_delete)
    .resource(user_resource).resource(post_resource)
    // ... 50 more lines

// ...compose from focused modules
let router = McpRouter::new()
    .merge(users::router())
    .merge(posts::router())
    .merge(comments::router());
```

## Core API

### merge() - Flat Combination

Combines all components from another router without modification.

```rust
impl<S> McpRouter<S> {
    /// Merge another router's components into this one.
    ///
    /// Tools, resources, prompts, and resource templates are all merged.
    /// Name collisions result in an error - use `merge_overwrite()` for
    /// last-wins behavior.
    ///
    /// # Example
    ///
    /// ```rust
    /// let admin_router = McpRouter::new()
    ///     .tool(create_user)
    ///     .tool(delete_user);
    ///
    /// let query_router = McpRouter::new()
    ///     .tool(search_users)
    ///     .tool(list_users);
    ///
    /// let router = McpRouter::new()
    ///     .server_info("my-server", "1.0.0")
    ///     .merge(admin_router)
    ///     .merge(query_router);
    ///
    /// // Result: router has all 4 tools
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if any component names collide.
    pub fn merge(self, other: McpRouter<S>) -> Result<Self, MergeError> {
        // ...
    }

    /// Merge with last-wins collision handling.
    ///
    /// If a component name exists in both routers, the one from `other`
    /// replaces the existing one.
    pub fn merge_overwrite(self, other: McpRouter<S>) -> Self {
        // ...
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("tool name collision: {0}")]
    ToolCollision(String),

    #[error("resource URI collision: {0}")]
    ResourceCollision(String),

    #[error("prompt name collision: {0}")]
    PromptCollision(String),

    #[error("resource template collision: {0}")]
    TemplateCollision(String),
}
```

### nest() - Namespaced Combination

Adds a prefix to all component names from the nested router.

```rust
impl<S> McpRouter<S> {
    /// Nest another router with a namespace prefix.
    ///
    /// All component names are prefixed with `prefix.` (dot separator).
    ///
    /// # Example
    ///
    /// ```rust
    /// let user_tools = McpRouter::new()
    ///     .tool(create)   // name: "create"
    ///     .tool(delete);  // name: "delete"
    ///
    /// let post_tools = McpRouter::new()
    ///     .tool(create)   // name: "create" (same name, different router)
    ///     .tool(delete);  // name: "delete"
    ///
    /// let router = McpRouter::new()
    ///     .nest("users", user_tools)   // tools: users.create, users.delete
    ///     .nest("posts", post_tools);  // tools: posts.create, posts.delete
    ///
    /// // No collision - names are prefixed
    /// ```
    ///
    /// # Resources
    ///
    /// Resource URIs are also prefixed:
    /// - `config://settings` becomes `config://users.settings` when nested under "users"
    ///
    /// # Errors
    ///
    /// Returns an error if prefixed names still collide.
    pub fn nest(self, prefix: &str, other: McpRouter<S>) -> Result<Self, MergeError> {
        // ...
    }
}
```

### Separator Choice

Using `.` (dot) as the separator:

```rust
// Nested tool names
"users.create"
"users.delete"
"posts.comments.create"  // nested nesting works

// Nested resource URIs
"config://users.settings"
"data://posts.recent"
```

Why dot:
- MCP tool names allow dots (unlike `/` which might conflict with URI semantics)
- Familiar from module namespacing (`std.fs.read`)
- Readable in tool lists

Alternative considered: `::` (Rust-style) - but longer and less common in JSON APIs.

## State Handling

### Same State Type

Simple case - both routers share state type:

```rust
let router_a: McpRouter<AppState> = /* ... */;
let router_b: McpRouter<AppState> = /* ... */;

let combined: McpRouter<AppState> = McpRouter::new()
    .with_state(app_state)
    .merge(router_a)?
    .merge(router_b)?;
```

### Different State Types

When routers have different state, we need state extraction:

```rust
// Option A: State must implement From/Into
let db_router: McpRouter<Database> = /* ... */;
let cache_router: McpRouter<Cache> = /* ... */;

// Won't work - state types don't match
// router.merge(db_router).merge(cache_router)

// Option B: Unified state with AsRef
struct AppState {
    db: Database,
    cache: Cache,
}

impl AsRef<Database> for AppState {
    fn as_ref(&self) -> &Database { &self.db }
}

impl AsRef<Cache> for AppState {
    fn as_ref(&self) -> &Cache { &self.cache }
}

// With extractors (#282), handlers use State<Database> which
// extracts via AsRef, so this works:
let router: McpRouter<AppState> = McpRouter::new()
    .with_state(app_state)
    .merge(db_router.map_state(|s: &AppState| &s.db))?
    .merge(cache_router.map_state(|s: &AppState| &s.cache))?;
```

### map_state()

Convert a router's state type:

```rust
impl<S> McpRouter<S> {
    /// Transform the state type of this router.
    ///
    /// Useful when merging routers with different state types into
    /// a router with a unified state.
    pub fn map_state<S2, F>(self, f: F) -> McpRouter<S2>
    where
        F: Fn(&S2) -> &S + Clone + Send + Sync + 'static,
    {
        // Wrap handlers to extract S from S2
    }
}
```

## Server Metadata

Only the root router's metadata applies:

```rust
let module_router = McpRouter::new()
    .server_info("ignored", "0.0.0")  // This is ignored when merged
    .instructions("also ignored")
    .tool(some_tool);

let root = McpRouter::new()
    .server_info("my-server", "1.0.0")  // This is used
    .instructions("This server does...")
    .merge(module_router)?;

// Client sees: server_info = ("my-server", "1.0.0")
```

Rationale: Server identity comes from the composed whole, not the parts.

## Capability Filters

Filters are also merged:

```rust
let admin_router = McpRouter::new()
    .tool(admin_tool)
    .tool_filter(require_admin_role);

let public_router = McpRouter::new()
    .tool(public_tool);
    // no filter - available to all

let router = McpRouter::new()
    .merge(admin_router)?   // filter applies to admin_tool
    .merge(public_router)?; // public_tool has no filter
```

Filters are per-component, so merging just brings them along.

## Implementation Sketch

```rust
pub struct McpRouter<S = ()> {
    server_info: Option<(String, String)>,
    instructions: Option<String>,
    tools: HashMap<String, Tool<S>>,
    resources: HashMap<String, Resource<S>>,
    prompts: HashMap<String, Prompt<S>>,
    templates: HashMap<String, ResourceTemplate<S>>,
    tool_filter: Option<ToolFilter>,
    resource_filter: Option<ResourceFilter>,
    prompt_filter: Option<PromptFilter>,
    // ...
}

impl<S> McpRouter<S> {
    pub fn merge(mut self, other: McpRouter<S>) -> Result<Self, MergeError> {
        // Check for collisions
        for name in other.tools.keys() {
            if self.tools.contains_key(name) {
                return Err(MergeError::ToolCollision(name.clone()));
            }
        }
        // ... same for resources, prompts, templates

        // Merge components
        self.tools.extend(other.tools);
        self.resources.extend(other.resources);
        self.prompts.extend(other.prompts);
        self.templates.extend(other.templates);

        // Merge filters (combine with OR - if either allows, allow)
        self.tool_filter = match (self.tool_filter, other.tool_filter) {
            (None, None) => None,
            (Some(f), None) | (None, Some(f)) => Some(f),
            (Some(f1), Some(f2)) => Some(f1.or(f2)),
        };
        // ... same for other filters

        // Ignore other's server_info and instructions
        Ok(self)
    }

    pub fn nest(self, prefix: &str, other: McpRouter<S>) -> Result<Self, MergeError> {
        // Prefix all names
        let prefixed_tools: HashMap<String, Tool<S>> = other.tools
            .into_iter()
            .map(|(name, tool)| (format!("{}.{}", prefix, name), tool.with_name(format!("{}.{}", prefix, name))))
            .collect();

        // ... same for resources, prompts, templates

        // Then merge
        let prefixed = McpRouter {
            tools: prefixed_tools,
            // ...
        };

        self.merge(prefixed)
    }
}
```

## Usage Patterns

### Modular Organization

```rust
// src/tools/users.rs
pub fn router() -> McpRouter<AppState> {
    McpRouter::new()
        .tool(create_user())
        .tool(delete_user())
        .tool(list_users())
}

// src/tools/posts.rs
pub fn router() -> McpRouter<AppState> {
    McpRouter::new()
        .tool(create_post())
        .tool(delete_post())
        .resource(recent_posts())
}

// src/main.rs
let router = McpRouter::new()
    .server_info("blog-api", "1.0.0")
    .with_state(app_state)
    .merge(users::router())?
    .merge(posts::router())?;
```

### Plugin Architecture

```rust
trait McpPlugin {
    fn name(&self) -> &str;
    fn router(&self) -> McpRouter<PluginContext>;
}

fn load_plugins(plugins: Vec<Box<dyn McpPlugin>>) -> McpRouter<AppState> {
    let mut router = McpRouter::new()
        .server_info("extensible-server", "1.0.0");

    for plugin in plugins {
        router = router
            .nest(plugin.name(), plugin.router().map_state(|s| &s.plugin_ctx))
            .expect("plugin name collision");
    }

    router
}
```

### Testing in Isolation

```rust
#[cfg(test)]
mod tests {
    use super::users;

    #[tokio::test]
    async fn test_user_tools() {
        let router = users::router();
        let client = TestClient::new(router);

        // Test just the user module
        let result = client.call_tool("create", json!({"name": "test"})).await;
        assert!(result.is_ok());
    }
}
```

## Interaction with #282 (Extractors)

Extractors make state handling cleaner:

```rust
// Without extractors - state type must match exactly
let router_a: McpRouter<Database> = /* ... */;
let router_b: McpRouter<Cache> = /* ... */;
// Can't merge - different state types

// With extractors - handlers use State<T> which extracts via AsRef
// So both can merge into McpRouter<AppState> if AppState: AsRef<Database> + AsRef<Cache>

let router_a = McpRouter::new()
    .tool(ToolBuilder::new("query")
        .handler(|State(db): State<Database>, input: Query| async { ... }));

let router_b = McpRouter::new()
    .tool(ToolBuilder::new("cached_query")
        .handler(|State(cache): State<Cache>, input: Query| async { ... }));

// Both work with AppState because extractors use AsRef
let combined: McpRouter<AppState> = McpRouter::new()
    .with_state(app_state)
    .merge(router_a)?
    .merge(router_b)?;
```

This is why #282 should land first.

## Open Questions

1. **Filter merging**: Should filters OR or AND?
   - OR: if either router allows, allow (proposed above)
   - AND: both must allow
   - Recommendation: OR, but provide `.merge_with_filter_strategy()`

2. **Notification senders**: How to handle `with_notification_sender()`?
   - Share the same sender across merged routers
   - Recommendation: Must be set on root only, merged routers inherit

3. **Completion handlers**: Merge or root-only?
   - Recommendation: Merge - each router can contribute completions

4. **Fluent vs Result**: Should `merge()` return `Result` or panic on collision?
   - Recommendation: Result for safety, add `merge_unchecked()` for convenience

5. **Nested filter scoping**: Should filters in nested routers apply to prefixed names?
   - Recommendation: Yes, filter sees the prefixed name
