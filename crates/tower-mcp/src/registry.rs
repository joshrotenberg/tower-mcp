//! Dynamic tool registry for runtime tool (de)registration.
//!
//! The [`DynamicToolRegistry`] provides a thread-safe, cloneable handle for
//! adding and removing tools at runtime. When tools change, all connected
//! sessions are notified via `notifications/tools/list_changed`.
//!
//! [`DynamicPromptRegistry`], [`DynamicResourceRegistry`] and
//! [`DynamicResourceTemplateRegistry`] do the same for prompts, resources and
//! resource templates.
//!
//! # Example
//!
//! ```rust
//! use tower_mcp::{McpRouter, ToolBuilder, CallToolResult};
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(Debug, Deserialize, JsonSchema)]
//! struct Input { value: String }
//!
//! let (router, registry) = McpRouter::new()
//!     .server_info("my-server", "1.0.0")
//!     .with_dynamic_tools();
//!
//! // Register a tool at runtime
//! let tool = ToolBuilder::new("echo")
//!     .description("Echo input")
//!     .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
//!     .build();
//!
//! registry.register(tool);
//! ```
//!
//! # Replacing a whole set
//!
//! `register` and `unregister` each broadcast one `list_changed`, so reloading
//! a set item by item sends one notification per call, and a client listing in
//! between sees part of the old set and part of the new. Every registry also
//! has `replace_all`, which swaps the whole set in one write, broadcasts once,
//! and returns a [`RegistryChange`] naming what was added, removed and
//! replaced:
//!
//! ```rust
//! use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};
//!
//! fn tool(name: &str) -> tower_mcp::Tool {
//!     ToolBuilder::new(name)
//!         .description("Reloadable tool")
//!         .no_params_handler(|| async { Ok(CallToolResult::text("ok")) })
//!         .build()
//! }
//!
//! let (_router, registry) = McpRouter::new()
//!     .server_info("my-server", "1.0.0")
//!     .with_dynamic_tools();
//! registry.register(tool("lint"));
//! registry.register(tool("format"));
//!
//! // A reload: `lint` stays (with a new handler), `format` goes, `test` is new.
//! let change = registry.replace_all([tool("lint"), tool("test")]);
//! assert_eq!(change.added, ["test"]);
//! assert_eq!(change.removed, ["format"]);
//! assert_eq!(change.replaced, ["lint"]);
//! ```

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, RwLock};

use crate::context::{NotificationSender, ServerNotification};
use crate::tool::Tool;

/// What a `replace_all` call changed, by key.
///
/// Keys are tool names, prompt names, resource URIs, and resource template URI
/// patterns, depending on the registry. Each list is sorted and holds each key
/// once.
///
/// Handlers cannot be compared, so every key present both before and after the
/// call is reported in [`replaced`](Self::replaced), whether or not its
/// definition changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RegistryChange {
    /// Keys in the new set that were not registered before the call.
    pub added: Vec<String>,
    /// Keys registered before the call that are not in the new set.
    pub removed: Vec<String>,
    /// Keys registered before the call that are also in the new set.
    pub replaced: Vec<String>,
}

impl RegistryChange {
    fn between<'a>(
        previous: impl IntoIterator<Item = &'a String>,
        current: impl IntoIterator<Item = &'a String>,
    ) -> Self {
        let previous: BTreeSet<&String> = previous.into_iter().collect();
        let current: BTreeSet<&String> = current.into_iter().collect();
        Self {
            added: current
                .difference(&previous)
                .map(|key| (*key).clone())
                .collect(),
            removed: previous
                .difference(&current)
                .map(|key| (*key).clone())
                .collect(),
            replaced: current
                .intersection(&previous)
                .map(|key| (*key).clone())
                .collect(),
        }
    }
}

/// Inner state shared between the registry handle and the router.
pub(crate) struct DynamicToolsInner {
    tools: RwLock<HashMap<String, Arc<Tool>>>,
    notification_senders: RwLock<Vec<NotificationSender>>,
}

impl DynamicToolsInner {
    pub(crate) fn new() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            notification_senders: RwLock::new(Vec::new()),
        }
    }

    /// Register a notification sender for a new session.
    pub(crate) fn add_notification_sender(&self, sender: NotificationSender) {
        let mut senders = self.notification_senders.write().unwrap();
        senders.push(sender);
    }

    /// Broadcast `ToolsListChanged` to all sessions, lazily cleaning up closed channels.
    fn broadcast_tools_changed(&self) {
        let mut senders = self.notification_senders.write().unwrap();
        senders.retain(|tx| !tx.is_closed());
        for tx in senders.iter() {
            let _ = tx.try_send(ServerNotification::ToolsListChanged);
        }
    }

    /// Get a snapshot of all dynamic tools.
    pub(crate) fn list(&self) -> Vec<Arc<Tool>> {
        let tools = self.tools.read().unwrap();
        tools.values().cloned().collect()
    }

    /// Look up a dynamic tool by name.
    pub(crate) fn get(&self, name: &str) -> Option<Arc<Tool>> {
        let tools = self.tools.read().unwrap();
        tools.get(name).cloned()
    }

    /// Check if a dynamic tool exists.
    pub(crate) fn contains(&self, name: &str) -> bool {
        let tools = self.tools.read().unwrap();
        tools.contains_key(name)
    }
}

/// A thread-safe, cloneable handle for runtime tool management.
///
/// Obtained from [`McpRouter::with_dynamic_tools()`](crate::McpRouter::with_dynamic_tools).
/// Tools registered here are merged with the router's static tools when
/// handling `tools/list` and `tools/call` requests.
///
/// When a tool is registered or unregistered, all connected sessions receive a
/// `notifications/tools/list_changed` notification.
/// [`replace_all`](Self::replace_all) swaps the whole set and sends one.
#[derive(Clone)]
pub struct DynamicToolRegistry {
    inner: Arc<DynamicToolsInner>,
}

impl DynamicToolRegistry {
    pub(crate) fn new(inner: Arc<DynamicToolsInner>) -> Self {
        Self { inner }
    }

    /// Register a tool, replacing any existing tool with the same name.
    ///
    /// Broadcasts `ToolsListChanged` to all connected sessions.
    pub fn register(&self, tool: Tool) {
        {
            let mut tools = self.inner.tools.write().unwrap();
            tools.insert(tool.name.clone(), Arc::new(tool));
        }
        self.inner.broadcast_tools_changed();
    }

    /// Unregister a tool by name.
    ///
    /// Returns `true` if the tool existed and was removed.
    /// Broadcasts `ToolsListChanged` only if the tool was actually removed.
    pub fn unregister(&self, name: &str) -> bool {
        let removed = {
            let mut tools = self.inner.tools.write().unwrap();
            tools.remove(name).is_some()
        };
        if removed {
            self.inner.broadcast_tools_changed();
        }
        removed
    }

    /// Replace every dynamic tool with `tools`, in one write.
    ///
    /// The new set is built before the lock is taken and swapped in under a
    /// single write, so `tools/list` returns either the old set or the new
    /// one, never a mix. If two tools share a name, the later one is kept, as
    /// with [`register`](Self::register).
    ///
    /// Broadcasts `ToolsListChanged` once, even when the set did not change.
    /// A call already running keeps the tool it started with.
    pub fn replace_all(&self, tools: impl IntoIterator<Item = Tool>) -> RegistryChange {
        let tools: HashMap<String, Arc<Tool>> = tools
            .into_iter()
            .map(|tool| (tool.name.clone(), Arc::new(tool)))
            .collect();
        let names: Vec<String> = tools.keys().cloned().collect();
        let previous = {
            let mut current = self.inner.tools.write().unwrap();
            std::mem::replace(&mut *current, tools)
        };
        self.inner.broadcast_tools_changed();
        RegistryChange::between(previous.keys(), &names)
    }

    /// List all currently registered dynamic tools.
    pub fn list(&self) -> Vec<Arc<Tool>> {
        self.inner.list()
    }

    /// Check if a tool with the given name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.inner.contains(name)
    }
}

// =============================================================================
// Dynamic Prompt Registry
// =============================================================================

/// Inner state shared between the prompt registry handle and the router.
pub(crate) struct DynamicPromptsInner {
    prompts: RwLock<HashMap<String, Arc<crate::prompt::Prompt>>>,
    notification_senders: RwLock<Vec<NotificationSender>>,
}

impl DynamicPromptsInner {
    pub(crate) fn new() -> Self {
        Self {
            prompts: RwLock::new(HashMap::new()),
            notification_senders: RwLock::new(Vec::new()),
        }
    }

    /// Register a notification sender for a new session.
    pub(crate) fn add_notification_sender(&self, sender: NotificationSender) {
        let mut senders = self.notification_senders.write().unwrap();
        senders.push(sender);
    }

    /// Broadcast `PromptsListChanged` to all sessions, lazily cleaning up closed channels.
    fn broadcast_prompts_changed(&self) {
        let mut senders = self.notification_senders.write().unwrap();
        senders.retain(|tx| !tx.is_closed());
        for tx in senders.iter() {
            let _ = tx.try_send(ServerNotification::PromptsListChanged);
        }
    }

    /// Get a snapshot of all dynamic prompts.
    pub(crate) fn list(&self) -> Vec<Arc<crate::prompt::Prompt>> {
        let prompts = self.prompts.read().unwrap();
        prompts.values().cloned().collect()
    }

    /// Look up a dynamic prompt by name.
    pub(crate) fn get(&self, name: &str) -> Option<Arc<crate::prompt::Prompt>> {
        let prompts = self.prompts.read().unwrap();
        prompts.get(name).cloned()
    }

    /// Check if a dynamic prompt exists.
    pub(crate) fn contains(&self, name: &str) -> bool {
        let prompts = self.prompts.read().unwrap();
        prompts.contains_key(name)
    }
}

/// A thread-safe, cloneable handle for runtime prompt management.
///
/// Obtained from [`McpRouter::with_dynamic_prompts()`](crate::McpRouter::with_dynamic_prompts).
/// Prompts registered here are merged with the router's static prompts when
/// handling `prompts/list` and `prompts/get` requests.
///
/// When a prompt is registered or unregistered, all connected sessions receive a
/// `notifications/prompts/list_changed` notification.
/// [`replace_all`](Self::replace_all) swaps the whole set and sends one.
///
/// # Example
///
/// ```rust
/// use tower_mcp::{McpRouter, PromptBuilder};
///
/// let (router, registry) = McpRouter::new()
///     .server_info("my-server", "1.0.0")
///     .with_dynamic_prompts();
///
/// // Register a prompt at runtime
/// let prompt = PromptBuilder::new("greet")
///     .description("Greet someone")
///     .user_message("Hello!");
///
/// registry.register(prompt);
/// ```
#[derive(Clone)]
pub struct DynamicPromptRegistry {
    inner: Arc<DynamicPromptsInner>,
}

impl DynamicPromptRegistry {
    pub(crate) fn new(inner: Arc<DynamicPromptsInner>) -> Self {
        Self { inner }
    }

    /// Register a prompt, replacing any existing prompt with the same name.
    ///
    /// Broadcasts `PromptsListChanged` to all connected sessions.
    pub fn register(&self, prompt: crate::prompt::Prompt) {
        {
            let mut prompts = self.inner.prompts.write().unwrap();
            prompts.insert(prompt.name.clone(), Arc::new(prompt));
        }
        self.inner.broadcast_prompts_changed();
    }

    /// Unregister a prompt by name.
    ///
    /// Returns `true` if the prompt existed and was removed.
    /// Broadcasts `PromptsListChanged` only if the prompt was actually removed.
    pub fn unregister(&self, name: &str) -> bool {
        let removed = {
            let mut prompts = self.inner.prompts.write().unwrap();
            prompts.remove(name).is_some()
        };
        if removed {
            self.inner.broadcast_prompts_changed();
        }
        removed
    }

    /// Replace every dynamic prompt with `prompts`, in one write.
    ///
    /// The new set is built before the lock is taken and swapped in under a
    /// single write, so `prompts/list` returns either the old set or the new
    /// one, never a mix. If two prompts share a name, the later one is kept,
    /// as with [`register`](Self::register).
    ///
    /// Broadcasts `PromptsListChanged` once, even when the set did not change.
    pub fn replace_all(
        &self,
        prompts: impl IntoIterator<Item = crate::prompt::Prompt>,
    ) -> RegistryChange {
        let prompts: HashMap<String, Arc<crate::prompt::Prompt>> = prompts
            .into_iter()
            .map(|prompt| (prompt.name.clone(), Arc::new(prompt)))
            .collect();
        let names: Vec<String> = prompts.keys().cloned().collect();
        let previous = {
            let mut current = self.inner.prompts.write().unwrap();
            std::mem::replace(&mut *current, prompts)
        };
        self.inner.broadcast_prompts_changed();
        RegistryChange::between(previous.keys(), &names)
    }

    /// List all currently registered dynamic prompts.
    pub fn list(&self) -> Vec<Arc<crate::prompt::Prompt>> {
        self.inner.list()
    }

    /// Check if a prompt with the given name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.inner.contains(name)
    }
}

// =============================================================================
// Dynamic Resource Registry
// =============================================================================

/// Inner state shared between the resource registry handle and the router.
pub(crate) struct DynamicResourcesInner {
    resources: RwLock<HashMap<String, Arc<crate::resource::Resource>>>,
    notification_senders: RwLock<Vec<NotificationSender>>,
}

impl DynamicResourcesInner {
    pub(crate) fn new() -> Self {
        Self {
            resources: RwLock::new(HashMap::new()),
            notification_senders: RwLock::new(Vec::new()),
        }
    }

    pub(crate) fn add_notification_sender(&self, sender: NotificationSender) {
        let mut senders = self.notification_senders.write().unwrap();
        senders.push(sender);
    }

    fn broadcast_resources_changed(&self) {
        let mut senders = self.notification_senders.write().unwrap();
        senders.retain(|tx| !tx.is_closed());
        for tx in senders.iter() {
            let _ = tx.try_send(ServerNotification::ResourcesListChanged);
        }
    }

    pub(crate) fn list(&self) -> Vec<Arc<crate::resource::Resource>> {
        let resources = self.resources.read().unwrap();
        resources.values().cloned().collect()
    }

    pub(crate) fn get(&self, uri: &str) -> Option<Arc<crate::resource::Resource>> {
        let resources = self.resources.read().unwrap();
        resources.get(uri).cloned()
    }
}

/// A thread-safe, cloneable handle for runtime resource management.
///
/// Obtained from [`McpRouter::with_dynamic_resources()`](crate::McpRouter::with_dynamic_resources).
/// Resources registered here are merged with the router's static resources
/// when handling `resources/list` and `resources/read` requests. Static
/// resources take precedence over dynamic resources when URIs collide.
///
/// When a resource is registered or unregistered, all connected sessions
/// receive a `notifications/resources/list_changed` notification.
/// [`replace_all`](Self::replace_all) swaps the whole set and sends one.
///
/// # Example
///
/// ```rust
/// use tower_mcp::{McpRouter, ResourceBuilder};
///
/// let (router, registry) = McpRouter::new()
///     .server_info("my-server", "1.0.0")
///     .with_dynamic_resources();
///
/// let resource = ResourceBuilder::new("file:///data.json")
///     .name("Data")
///     .text(r#"{"key": "value"}"#);
///
/// registry.register(resource);
/// ```
#[derive(Clone)]
pub struct DynamicResourceRegistry {
    inner: Arc<DynamicResourcesInner>,
}

impl DynamicResourceRegistry {
    pub(crate) fn new(inner: Arc<DynamicResourcesInner>) -> Self {
        Self { inner }
    }

    /// Register a resource, replacing any existing resource with the same URI.
    ///
    /// Broadcasts `ResourcesListChanged` to all connected sessions.
    pub fn register(&self, resource: crate::resource::Resource) {
        {
            let mut resources = self.inner.resources.write().unwrap();
            resources.insert(resource.uri.clone(), Arc::new(resource));
        }
        self.inner.broadcast_resources_changed();
    }

    /// Unregister a resource by URI.
    ///
    /// Returns `true` if the resource existed and was removed.
    /// Broadcasts `ResourcesListChanged` only if the resource was actually removed.
    pub fn unregister(&self, uri: &str) -> bool {
        let removed = {
            let mut resources = self.inner.resources.write().unwrap();
            resources.remove(uri).is_some()
        };
        if removed {
            self.inner.broadcast_resources_changed();
        }
        removed
    }

    /// Replace every dynamic resource with `resources`, in one write.
    ///
    /// The new set is built before the lock is taken and swapped in under a
    /// single write, so `resources/list` returns either the old set or the
    /// new one, never a mix. If two resources share a URI, the later one is
    /// kept, as with [`register`](Self::register).
    ///
    /// Broadcasts `ResourcesListChanged` once, even when the set did not
    /// change.
    pub fn replace_all(
        &self,
        resources: impl IntoIterator<Item = crate::resource::Resource>,
    ) -> RegistryChange {
        let resources: HashMap<String, Arc<crate::resource::Resource>> = resources
            .into_iter()
            .map(|resource| (resource.uri.clone(), Arc::new(resource)))
            .collect();
        let uris: Vec<String> = resources.keys().cloned().collect();
        let previous = {
            let mut current = self.inner.resources.write().unwrap();
            std::mem::replace(&mut *current, resources)
        };
        self.inner.broadcast_resources_changed();
        RegistryChange::between(previous.keys(), &uris)
    }

    /// List all currently registered dynamic resources.
    pub fn list(&self) -> Vec<Arc<crate::resource::Resource>> {
        self.inner.list()
    }

    /// Check if a resource with the given URI is registered.
    pub fn contains(&self, uri: &str) -> bool {
        let resources = self.inner.resources.read().unwrap();
        resources.contains_key(uri)
    }
}

// =============================================================================
// Dynamic Resource Template Registry
// =============================================================================

/// Inner state shared between the resource template registry handle and the router.
pub(crate) struct DynamicResourceTemplatesInner {
    templates: RwLock<Vec<Arc<crate::resource::ResourceTemplate>>>,
    notification_senders: RwLock<Vec<NotificationSender>>,
}

impl DynamicResourceTemplatesInner {
    pub(crate) fn new() -> Self {
        Self {
            templates: RwLock::new(Vec::new()),
            notification_senders: RwLock::new(Vec::new()),
        }
    }

    pub(crate) fn add_notification_sender(&self, sender: NotificationSender) {
        let mut senders = self.notification_senders.write().unwrap();
        senders.push(sender);
    }

    fn broadcast_resources_changed(&self) {
        let mut senders = self.notification_senders.write().unwrap();
        senders.retain(|tx| !tx.is_closed());
        for tx in senders.iter() {
            let _ = tx.try_send(ServerNotification::ResourcesListChanged);
        }
    }

    pub(crate) fn list(&self) -> Vec<Arc<crate::resource::ResourceTemplate>> {
        let templates = self.templates.read().unwrap();
        templates.clone()
    }

    pub(crate) fn match_uri(
        &self,
        uri: &str,
    ) -> Option<(
        Arc<crate::resource::ResourceTemplate>,
        std::collections::HashMap<String, String>,
    )> {
        let templates = self.templates.read().unwrap();
        for template in templates.iter() {
            if let Some(variables) = template.match_uri(uri) {
                return Some((Arc::clone(template), variables));
            }
        }
        None
    }
}

/// A thread-safe, cloneable handle for runtime resource template management.
///
/// Obtained from [`McpRouter::with_dynamic_resource_templates()`](crate::McpRouter::with_dynamic_resource_templates).
/// Templates registered here are merged with the router's static templates
/// when handling `resources/templates/list` and `resources/read` requests.
/// Static templates are checked before dynamic ones.
///
/// Protocol requests are subject to the router's
/// [`resource_template_filter`](crate::McpRouter::resource_template_filter).
/// That policy is evaluated again for every request, including against the
/// concrete URI selected by `resources/read`. If the router has a
/// [`resource_filter`](crate::McpRouter::resource_filter) but no resource
/// template filter, templates fail closed: they are omitted from protocol
/// listings and cannot be read.
///
/// The registry handle is an application-management view, not a client-scoped
/// protocol view. Its [`list`](Self::list) and [`contains`](Self::contains)
/// methods deliberately do not apply router filters; do not use their results
/// as an authorization decision or expose them directly to an untrusted
/// client.
///
/// When a template is registered or unregistered, all connected sessions
/// receive a `notifications/resources/list_changed` notification.
/// [`replace_all`](Self::replace_all) swaps the whole set and sends one.
///
/// # Example
///
/// ```rust,ignore
/// use tower_mcp::{McpRouter, ResourceTemplateBuilder};
///
/// let (router, registry) = McpRouter::new()
///     .server_info("my-server", "1.0.0")
///     .with_dynamic_resource_templates();
///
/// let template = ResourceTemplateBuilder::new("db://tables/{table}")
///     .name("Database Table")
///     .handler(|uri, vars| async move { /* ... */ });
///
/// registry.register(template);
/// ```
#[derive(Clone)]
pub struct DynamicResourceTemplateRegistry {
    inner: Arc<DynamicResourceTemplatesInner>,
}

impl DynamicResourceTemplateRegistry {
    pub(crate) fn new(inner: Arc<DynamicResourceTemplatesInner>) -> Self {
        Self { inner }
    }

    /// Register a resource template.
    ///
    /// Broadcasts `ResourcesListChanged` to all connected sessions.
    pub fn register(&self, template: crate::resource::ResourceTemplate) {
        {
            let mut templates = self.inner.templates.write().unwrap();
            // Remove any existing template with the same URI pattern
            templates.retain(|t| t.uri_template != template.uri_template);
            templates.push(Arc::new(template));
        }
        self.inner.broadcast_resources_changed();
    }

    /// Unregister a resource template by URI pattern.
    ///
    /// Returns `true` if the template existed and was removed.
    /// Broadcasts `ResourcesListChanged` only if the template was actually removed.
    pub fn unregister(&self, uri_template: &str) -> bool {
        let removed = {
            let mut templates = self.inner.templates.write().unwrap();
            let before = templates.len();
            templates.retain(|t| t.uri_template != uri_template);
            templates.len() < before
        };
        if removed {
            self.inner.broadcast_resources_changed();
        }
        removed
    }

    /// Replace every dynamic resource template with `templates`, in one write.
    ///
    /// The new set is built before the lock is taken and swapped in under a
    /// single write, so `resources/templates/list` and `resources/read` see
    /// either the old set or the new one, never a mix.
    ///
    /// Templates are matched in the order `templates` yields them. If two
    /// share a URI pattern, the later one is kept, at the later position, as
    /// with [`register`](Self::register).
    ///
    /// Broadcasts `ResourcesListChanged` once, even when the set did not
    /// change.
    pub fn replace_all(
        &self,
        templates: impl IntoIterator<Item = crate::resource::ResourceTemplate>,
    ) -> RegistryChange {
        let mut ordered: Vec<Arc<crate::resource::ResourceTemplate>> = Vec::new();
        for template in templates {
            ordered.retain(|t| t.uri_template != template.uri_template);
            ordered.push(Arc::new(template));
        }
        let patterns: Vec<String> = ordered.iter().map(|t| t.uri_template.clone()).collect();
        let previous = {
            let mut current = self.inner.templates.write().unwrap();
            std::mem::replace(&mut *current, ordered)
        };
        self.inner.broadcast_resources_changed();
        RegistryChange::between(previous.iter().map(|t| &t.uri_template), &patterns)
    }

    /// List all currently registered dynamic resource templates.
    ///
    /// This is an unfiltered application-management view. Visibility filters
    /// are applied only when the router handles protocol requests.
    pub fn list(&self) -> Vec<Arc<crate::resource::ResourceTemplate>> {
        self.inner.list()
    }

    /// Check if a template with the given URI pattern is registered.
    ///
    /// This reports registry state without consulting client visibility
    /// filters.
    pub fn contains(&self, uri_template: &str) -> bool {
        let templates = self.inner.templates.read().unwrap();
        templates.iter().any(|t| t.uri_template == uri_template)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CallToolResult;
    use crate::tool::ToolBuilder;
    use tokio::sync::mpsc;

    fn make_tool(name: &str) -> Tool {
        ToolBuilder::new(name)
            .description(format!("Test tool: {name}"))
            .no_params_handler(|| async { Ok(CallToolResult::text("ok")) })
            .build()
    }

    fn make_registry() -> (DynamicToolRegistry, Arc<DynamicToolsInner>) {
        let inner = Arc::new(DynamicToolsInner::new());
        let registry = DynamicToolRegistry::new(inner.clone());
        (registry, inner)
    }

    #[test]
    fn test_register_and_list() {
        let (registry, _) = make_registry();

        assert!(registry.list().is_empty());

        registry.register(make_tool("tool_a"));
        assert_eq!(registry.list().len(), 1);
        assert!(registry.contains("tool_a"));

        registry.register(make_tool("tool_b"));
        assert_eq!(registry.list().len(), 2);
        assert!(registry.contains("tool_b"));
    }

    #[test]
    fn test_unregister() {
        let (registry, _) = make_registry();

        registry.register(make_tool("tool_a"));
        registry.register(make_tool("tool_b"));
        assert_eq!(registry.list().len(), 2);

        assert!(registry.unregister("tool_a"));
        assert_eq!(registry.list().len(), 1);
        assert!(!registry.contains("tool_a"));
        assert!(registry.contains("tool_b"));
    }

    #[test]
    fn test_unregister_nonexistent() {
        let (registry, _) = make_registry();
        assert!(!registry.unregister("no_such_tool"));
    }

    #[test]
    fn test_register_replaces_existing() {
        let (registry, _) = make_registry();

        registry.register(make_tool("tool_a"));
        registry.register(make_tool("tool_a"));
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn test_contains() {
        let (registry, _) = make_registry();

        assert!(!registry.contains("tool_a"));
        registry.register(make_tool("tool_a"));
        assert!(registry.contains("tool_a"));
        registry.unregister("tool_a");
        assert!(!registry.contains("tool_a"));
    }

    #[test]
    fn test_inner_get() {
        let (registry, inner) = make_registry();

        assert!(inner.get("tool_a").is_none());
        registry.register(make_tool("tool_a"));
        let tool = inner.get("tool_a").unwrap();
        assert_eq!(tool.name, "tool_a");
    }

    #[tokio::test]
    async fn test_broadcast_on_register() {
        let (registry, inner) = make_registry();

        let (tx, mut rx) = mpsc::channel(16);
        inner.add_notification_sender(tx);

        registry.register(make_tool("tool_a"));

        let notification = rx.try_recv().unwrap();
        assert!(matches!(notification, ServerNotification::ToolsListChanged));
    }

    #[tokio::test]
    async fn test_broadcast_on_unregister() {
        let (registry, inner) = make_registry();

        registry.register(make_tool("tool_a"));

        let (tx, mut rx) = mpsc::channel(16);
        inner.add_notification_sender(tx);

        registry.unregister("tool_a");

        let notification = rx.try_recv().unwrap();
        assert!(matches!(notification, ServerNotification::ToolsListChanged));
    }

    #[tokio::test]
    async fn test_no_broadcast_on_unregister_nonexistent() {
        let (registry, inner) = make_registry();

        let (tx, mut rx) = mpsc::channel(16);
        inner.add_notification_sender(tx);

        registry.unregister("no_such_tool");

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_closed_senders_are_cleaned_up() {
        let (registry, inner) = make_registry();

        let (tx, rx) = mpsc::channel(16);
        inner.add_notification_sender(tx);
        // Drop the receiver to close the channel
        drop(rx);

        // This should not panic, and should clean up the closed sender
        registry.register(make_tool("tool_a"));

        // Add a new sender and verify it still works
        let (tx2, mut rx2) = mpsc::channel(16);
        inner.add_notification_sender(tx2);

        registry.register(make_tool("tool_b"));
        let notification = rx2.try_recv().unwrap();
        assert!(matches!(notification, ServerNotification::ToolsListChanged));
    }

    #[tokio::test]
    async fn test_replace_all_reports_change_and_broadcasts_once() {
        let (registry, inner) = make_registry();
        registry.register(make_tool("tool_a"));
        registry.register(make_tool("tool_b"));
        registry.register(make_tool("tool_c"));

        let (tx, mut rx) = mpsc::channel(16);
        inner.add_notification_sender(tx);

        let change = registry.replace_all([make_tool("tool_d"), make_tool("tool_b")]);
        assert_eq!(change.added, ["tool_d"]);
        assert_eq!(change.removed, ["tool_a", "tool_c"]);
        assert_eq!(change.replaced, ["tool_b"]);

        let mut names: Vec<String> = registry.list().iter().map(|t| t.name.clone()).collect();
        names.sort();
        assert_eq!(names, ["tool_b", "tool_d"]);

        assert!(matches!(
            rx.try_recv().unwrap(),
            ServerNotification::ToolsListChanged
        ));
        assert!(
            rx.try_recv().is_err(),
            "replace_all broadcasts exactly once"
        );
    }

    #[tokio::test]
    async fn test_replace_all_broadcasts_when_unchanged() {
        let (registry, inner) = make_registry();

        let (tx, mut rx) = mpsc::channel(16);
        inner.add_notification_sender(tx);

        let change = registry.replace_all(Vec::new());
        assert_eq!(change, RegistryChange::default());
        assert!(matches!(
            rx.try_recv().unwrap(),
            ServerNotification::ToolsListChanged
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_replace_all_keeps_last_duplicate() {
        let (registry, inner) = make_registry();

        let first = ToolBuilder::new("dup")
            .description("first")
            .no_params_handler(|| async { Ok(CallToolResult::text("first")) })
            .build();
        let second = ToolBuilder::new("dup")
            .description("second")
            .no_params_handler(|| async { Ok(CallToolResult::text("second")) })
            .build();

        let change = registry.replace_all([first, second]);
        assert_eq!(change.added, ["dup"]);
        assert!(change.removed.is_empty());
        assert!(change.replaced.is_empty());
        assert_eq!(registry.list().len(), 1);
        assert_eq!(
            inner.get("dup").unwrap().description.as_deref(),
            Some("second")
        );
    }

    #[test]
    fn test_replace_all_keeps_in_flight_tool() {
        let (registry, inner) = make_registry();
        registry.register(make_tool("tool_a"));

        // A call holds the Arc it looked up; a reload does not take it away.
        let held = inner.get("tool_a").unwrap();
        registry.replace_all(Vec::new());
        assert!(!registry.contains("tool_a"));
        assert_eq!(held.name, "tool_a");
    }

    // =========================================================================
    // DynamicPromptRegistry tests
    // =========================================================================

    fn make_prompt(name: &str) -> crate::prompt::Prompt {
        crate::prompt::PromptBuilder::new(name)
            .description(format!("Test prompt: {name}"))
            .user_message("ok")
    }

    fn make_prompt_registry() -> (DynamicPromptRegistry, Arc<DynamicPromptsInner>) {
        let inner = Arc::new(DynamicPromptsInner::new());
        let registry = DynamicPromptRegistry::new(inner.clone());
        (registry, inner)
    }

    #[test]
    fn test_prompt_register_and_list() {
        let (registry, _) = make_prompt_registry();

        assert!(registry.list().is_empty());

        registry.register(make_prompt("prompt_a"));
        assert_eq!(registry.list().len(), 1);
        assert!(registry.contains("prompt_a"));

        registry.register(make_prompt("prompt_b"));
        assert_eq!(registry.list().len(), 2);
        assert!(registry.contains("prompt_b"));
    }

    #[test]
    fn test_prompt_unregister() {
        let (registry, _) = make_prompt_registry();

        registry.register(make_prompt("prompt_a"));
        registry.register(make_prompt("prompt_b"));

        assert!(registry.unregister("prompt_a"));
        assert_eq!(registry.list().len(), 1);
        assert!(!registry.contains("prompt_a"));
        assert!(registry.contains("prompt_b"));
    }

    #[test]
    fn test_prompt_unregister_nonexistent() {
        let (registry, _) = make_prompt_registry();
        assert!(!registry.unregister("no_such_prompt"));
    }

    #[tokio::test]
    async fn test_prompt_broadcast_on_register() {
        let (registry, inner) = make_prompt_registry();

        let (tx, mut rx) = mpsc::channel(16);
        inner.add_notification_sender(tx);

        registry.register(make_prompt("prompt_a"));

        let notification = rx.try_recv().unwrap();
        assert!(matches!(
            notification,
            ServerNotification::PromptsListChanged
        ));
    }

    #[tokio::test]
    async fn test_prompt_broadcast_on_unregister() {
        let (registry, inner) = make_prompt_registry();

        registry.register(make_prompt("prompt_a"));

        let (tx, mut rx) = mpsc::channel(16);
        inner.add_notification_sender(tx);

        registry.unregister("prompt_a");

        let notification = rx.try_recv().unwrap();
        assert!(matches!(
            notification,
            ServerNotification::PromptsListChanged
        ));
    }

    #[tokio::test]
    async fn test_prompt_replace_all() {
        let (registry, inner) = make_prompt_registry();
        registry.register(make_prompt("prompt_a"));
        registry.register(make_prompt("prompt_b"));

        let (tx, mut rx) = mpsc::channel(16);
        inner.add_notification_sender(tx);

        let change = registry.replace_all([make_prompt("prompt_b"), make_prompt("prompt_c")]);
        assert_eq!(change.added, ["prompt_c"]);
        assert_eq!(change.removed, ["prompt_a"]);
        assert_eq!(change.replaced, ["prompt_b"]);
        assert!(!registry.contains("prompt_a"));
        assert!(registry.contains("prompt_c"));

        assert!(matches!(
            rx.try_recv().unwrap(),
            ServerNotification::PromptsListChanged
        ));
        assert!(rx.try_recv().is_err());
    }

    // =========================================================================
    // DynamicResourceRegistry tests
    // =========================================================================

    fn make_resource(uri: &str) -> crate::resource::Resource {
        crate::resource::ResourceBuilder::new(uri)
            .name(uri)
            .text("content")
    }

    fn make_resource_registry() -> (DynamicResourceRegistry, Arc<DynamicResourcesInner>) {
        let inner = Arc::new(DynamicResourcesInner::new());
        let registry = DynamicResourceRegistry::new(inner.clone());
        (registry, inner)
    }

    #[test]
    fn test_resource_register_and_list() {
        let (registry, _) = make_resource_registry();

        assert!(registry.list().is_empty());

        registry.register(make_resource("file:///a.txt"));
        assert_eq!(registry.list().len(), 1);
        assert!(registry.contains("file:///a.txt"));

        registry.register(make_resource("file:///b.txt"));
        assert_eq!(registry.list().len(), 2);
    }

    #[test]
    fn test_resource_unregister() {
        let (registry, _) = make_resource_registry();

        registry.register(make_resource("file:///a.txt"));
        registry.register(make_resource("file:///b.txt"));

        assert!(registry.unregister("file:///a.txt"));
        assert_eq!(registry.list().len(), 1);
        assert!(!registry.contains("file:///a.txt"));
        assert!(registry.contains("file:///b.txt"));
    }

    #[test]
    fn test_resource_unregister_nonexistent() {
        let (registry, _) = make_resource_registry();
        assert!(!registry.unregister("file:///nope"));
    }

    #[tokio::test]
    async fn test_resource_broadcast_on_register() {
        let (registry, inner) = make_resource_registry();

        let (tx, mut rx) = mpsc::channel(16);
        inner.add_notification_sender(tx);

        registry.register(make_resource("file:///a.txt"));

        let notification = rx.try_recv().unwrap();
        assert!(matches!(
            notification,
            ServerNotification::ResourcesListChanged
        ));
    }

    #[tokio::test]
    async fn test_resource_replace_all() {
        let (registry, inner) = make_resource_registry();
        registry.register(make_resource("file:///a.txt"));
        registry.register(make_resource("file:///b.txt"));

        let (tx, mut rx) = mpsc::channel(16);
        inner.add_notification_sender(tx);

        let change = registry.replace_all([
            make_resource("file:///c.txt"),
            make_resource("file:///a.txt"),
        ]);
        assert_eq!(change.added, ["file:///c.txt"]);
        assert_eq!(change.removed, ["file:///b.txt"]);
        assert_eq!(change.replaced, ["file:///a.txt"]);
        assert!(!registry.contains("file:///b.txt"));
        assert!(inner.get("file:///c.txt").is_some());

        assert!(matches!(
            rx.try_recv().unwrap(),
            ServerNotification::ResourcesListChanged
        ));
        assert!(rx.try_recv().is_err());
    }

    // =========================================================================
    // DynamicResourceTemplateRegistry tests
    // =========================================================================

    fn make_template_registry() -> (
        DynamicResourceTemplateRegistry,
        Arc<DynamicResourceTemplatesInner>,
    ) {
        let inner = Arc::new(DynamicResourceTemplatesInner::new());
        let registry = DynamicResourceTemplateRegistry::new(inner.clone());
        (registry, inner)
    }

    #[test]
    fn test_template_register_and_list() {
        use crate::resource::ResourceTemplateBuilder;

        let (registry, _) = make_template_registry();
        assert!(registry.list().is_empty());

        let template = ResourceTemplateBuilder::new("db://tables/{table}")
            .name("Tables")
            .handler(
                |uri: String, _vars: std::collections::HashMap<String, String>| async move {
                    Ok(crate::protocol::ReadResourceResult {
                        contents: vec![crate::protocol::ResourceContent {
                            uri,
                            mime_type: None,
                            text: Some("data".to_string()),
                            blob: None,
                            meta: None,
                        }],
                        meta: None,
                        ..Default::default()
                    })
                },
            );

        registry.register(template);
        assert_eq!(registry.list().len(), 1);
        assert!(registry.contains("db://tables/{table}"));
    }

    #[test]
    fn test_template_unregister() {
        use crate::resource::ResourceTemplateBuilder;

        let (registry, _) = make_template_registry();

        let template = ResourceTemplateBuilder::new("db://tables/{table}")
            .name("Tables")
            .handler(
                |uri: String, _vars: std::collections::HashMap<String, String>| async move {
                    Ok(crate::protocol::ReadResourceResult {
                        contents: vec![crate::protocol::ResourceContent {
                            uri,
                            mime_type: None,
                            text: Some("data".to_string()),
                            blob: None,
                            meta: None,
                        }],
                        meta: None,
                        ..Default::default()
                    })
                },
            );

        registry.register(template);
        assert!(registry.unregister("db://tables/{table}"));
        assert!(registry.list().is_empty());
        assert!(!registry.unregister("db://tables/{table}"));
    }

    #[tokio::test]
    async fn test_template_broadcast_on_register() {
        use crate::resource::ResourceTemplateBuilder;

        let (registry, inner) = make_template_registry();

        let (tx, mut rx) = mpsc::channel(16);
        inner.add_notification_sender(tx);

        let template = ResourceTemplateBuilder::new("db://tables/{table}")
            .name("Tables")
            .handler(
                |uri: String, _vars: std::collections::HashMap<String, String>| async move {
                    Ok(crate::protocol::ReadResourceResult {
                        contents: vec![crate::protocol::ResourceContent {
                            uri,
                            mime_type: None,
                            text: Some("data".to_string()),
                            blob: None,
                            meta: None,
                        }],
                        meta: None,
                        ..Default::default()
                    })
                },
            );

        registry.register(template);

        let notification = rx.try_recv().unwrap();
        assert!(matches!(
            notification,
            ServerNotification::ResourcesListChanged
        ));
    }

    #[tokio::test]
    async fn test_template_match_uri() {
        use crate::resource::ResourceTemplateBuilder;

        let (_, inner) = make_template_registry();

        let template = ResourceTemplateBuilder::new("db://tables/{table}")
            .name("Tables")
            .handler(
                |uri: String, _vars: std::collections::HashMap<String, String>| async move {
                    Ok(crate::protocol::ReadResourceResult {
                        contents: vec![crate::protocol::ResourceContent {
                            uri,
                            mime_type: None,
                            text: Some("data".to_string()),
                            blob: None,
                            meta: None,
                        }],
                        meta: None,
                        ..Default::default()
                    })
                },
            );

        {
            let mut templates = inner.templates.write().unwrap();
            templates.push(Arc::new(template));
        }

        let result = inner.match_uri("db://tables/users");
        assert!(result.is_some());
        let (_, vars) = result.unwrap();
        assert_eq!(vars.get("table").unwrap(), "users");

        assert!(inner.match_uri("db://other/path").is_none());
    }

    fn make_template(pattern: &str, body: &'static str) -> crate::resource::ResourceTemplate {
        crate::resource::ResourceTemplateBuilder::new(pattern)
            .name(pattern)
            .handler(
                move |uri: String, _vars: std::collections::HashMap<String, String>| async move {
                    Ok(crate::protocol::ReadResourceResult::text(uri, body))
                },
            )
    }

    #[tokio::test]
    async fn test_template_replace_all() {
        let (registry, inner) = make_template_registry();
        registry.register(make_template("db://tables/{table}", "table"));
        registry.register(make_template("db://views/{view}", "view"));

        let (tx, mut rx) = mpsc::channel(16);
        inner.add_notification_sender(tx);

        let change = registry.replace_all([
            make_template("db://tables/{table}", "table v2"),
            make_template("db://indexes/{index}", "index"),
        ]);
        assert_eq!(change.added, ["db://indexes/{index}"]);
        assert_eq!(change.removed, ["db://views/{view}"]);
        assert_eq!(change.replaced, ["db://tables/{table}"]);

        assert!(matches!(
            rx.try_recv().unwrap(),
            ServerNotification::ResourcesListChanged
        ));
        assert!(rx.try_recv().is_err());

        let (template, vars) = inner.match_uri("db://tables/users").unwrap();
        let result = template.read("db://tables/users", vars).await.unwrap();
        assert_eq!(result.contents[0].text.as_deref(), Some("table v2"));
        assert!(inner.match_uri("db://views/v").is_none());
    }

    #[tokio::test]
    async fn test_template_replace_all_keeps_given_order() {
        let (registry, inner) = make_template_registry();

        // Both patterns match `db://users/1`; the first one listed wins.
        registry.replace_all([
            make_template("db://users/{id}", "narrow"),
            make_template("db://{+rest}", "wide"),
        ]);
        let (template, _) = inner.match_uri("db://users/1").unwrap();
        assert_eq!(template.uri_template, "db://users/{id}");

        // A repeated pattern keeps the later one, at the later position.
        let change = registry.replace_all([
            make_template("db://users/{id}", "first"),
            make_template("db://{+rest}", "wide"),
            make_template("db://users/{id}", "last"),
        ]);
        assert_eq!(change.replaced, ["db://users/{id}", "db://{+rest}"]);
        let patterns: Vec<String> = registry
            .list()
            .iter()
            .map(|t| t.uri_template.clone())
            .collect();
        assert_eq!(patterns, ["db://{+rest}", "db://users/{id}"]);
        let (template, _) = inner.match_uri("db://users/1").unwrap();
        assert_eq!(template.uri_template, "db://{+rest}");
    }
}
