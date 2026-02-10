//! Dynamic tool registry for runtime tool (de)registration.
//!
//! The [`DynamicToolRegistry`] provides a thread-safe, cloneable handle for
//! adding and removing tools at runtime. When tools change, all connected
//! sessions are notified via `notifications/tools/list_changed`.
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

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::context::{NotificationSender, ServerNotification};
use crate::tool::Tool;

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

    /// List all currently registered dynamic tools.
    pub fn list(&self) -> Vec<Arc<Tool>> {
        self.inner.list()
    }

    /// Check if a tool with the given name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.inner.contains(name)
    }
}
