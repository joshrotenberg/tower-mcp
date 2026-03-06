//! Backend connection management.
//!
//! Each backend wraps an [`McpClient`] with cached capability discovery
//! and namespace prefixing. Backends automatically refresh their cache
//! when the server sends list-changed notifications.

use std::sync::Arc;

use tokio::sync::{RwLock, mpsc};

use crate::client::{ClientTransport, McpClient, NotificationHandler};
use crate::error::Result;
use crate::protocol::{
    CallToolResult, GetPromptResult, PromptDefinition, ReadResourceResult, ResourceDefinition,
    ResourceTemplateDefinition, ToolDefinition,
};

/// Cached capabilities from a backend server.
#[derive(Debug, Default, Clone)]
pub(crate) struct CachedCapabilities {
    pub tools: Vec<ToolDefinition>,
    pub resources: Vec<ResourceDefinition>,
    pub resource_templates: Vec<ResourceTemplateDefinition>,
    pub prompts: Vec<PromptDefinition>,
}

/// What kind of capability list changed.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ListChanged {
    Tools,
    Resources,
    Prompts,
}

/// A connected backend MCP server with namespace and cached capabilities.
pub(crate) struct Backend {
    /// Namespace prefix for this backend's capabilities.
    pub namespace: String,
    /// Separator between namespace and name (default: `_`).
    pub separator: String,
    /// The connected MCP client.
    pub client: McpClient,
    /// Cached capabilities, refreshed on list-changed notifications.
    pub cache: Arc<RwLock<CachedCapabilities>>,
}

impl Backend {
    /// Connect a transport and create a backend with a notification handler
    /// that sends invalidation signals on the provided channel.
    pub async fn connect(
        namespace: impl Into<String>,
        transport: impl ClientTransport,
        separator: String,
        invalidation_tx: mpsc::Sender<ListChanged>,
    ) -> Result<Self> {
        let tools_tx = invalidation_tx.clone();
        let resources_tx = invalidation_tx.clone();
        let prompts_tx = invalidation_tx;

        let handler = NotificationHandler::new()
            .on_tools_changed(move || {
                let _ = tools_tx.try_send(ListChanged::Tools);
            })
            .on_resources_changed(move || {
                let _ = resources_tx.try_send(ListChanged::Resources);
            })
            .on_prompts_changed(move || {
                let _ = prompts_tx.try_send(ListChanged::Prompts);
            });

        let client = McpClient::connect_with_handler(transport, handler).await?;
        let cache = Arc::new(RwLock::new(CachedCapabilities::default()));

        Ok(Self {
            namespace: namespace.into(),
            separator,
            client,
            cache,
        })
    }

    /// Create a backend from an already-connected client (no notification forwarding).
    pub fn from_client(namespace: impl Into<String>, client: McpClient, separator: String) -> Self {
        Self {
            namespace: namespace.into(),
            separator,
            client,
            cache: Arc::new(RwLock::new(CachedCapabilities::default())),
        }
    }

    /// Initialize the backend: run MCP initialize handshake and discover capabilities.
    pub async fn initialize(&self, proxy_name: &str, proxy_version: &str) -> Result<()> {
        self.client.initialize(proxy_name, proxy_version).await?;
        self.refresh_capabilities().await?;
        Ok(())
    }

    /// Refresh cached capabilities from the backend.
    pub async fn refresh_capabilities(&self) -> Result<()> {
        // Fetch all capabilities concurrently
        let (tools, resources, templates, prompts) = tokio::join!(
            self.client.list_all_tools(),
            self.client.list_all_resources(),
            self.client.list_all_resource_templates(),
            self.client.list_all_prompts(),
        );

        let mut cache = self.cache.write().await;
        cache.tools = tools.unwrap_or_default();
        cache.resources = resources.unwrap_or_default();
        cache.resource_templates = templates.unwrap_or_default();
        cache.prompts = prompts.unwrap_or_default();

        Ok(())
    }

    /// Refresh only the tools cache.
    pub async fn refresh_tools(&self) {
        if let Ok(tools) = self.client.list_all_tools().await {
            self.cache.write().await.tools = tools;
        }
    }

    /// Refresh only the resources cache.
    pub async fn refresh_resources(&self) {
        let (resources, templates) = tokio::join!(
            self.client.list_all_resources(),
            self.client.list_all_resource_templates(),
        );
        let mut cache = self.cache.write().await;
        if let Ok(r) = resources {
            cache.resources = r;
        }
        if let Ok(t) = templates {
            cache.resource_templates = t;
        }
    }

    /// Refresh only the prompts cache.
    pub async fn refresh_prompts(&self) {
        if let Ok(prompts) = self.client.list_all_prompts().await {
            self.cache.write().await.prompts = prompts;
        }
    }

    /// Prefix a name with this backend's namespace.
    pub fn prefixed_name(&self, name: &str) -> String {
        format!("{}{}{}", self.namespace, self.separator, name)
    }

    /// Strip the namespace prefix from a name, if it matches.
    pub fn strip_prefix<'a>(&self, name: &'a str) -> Option<&'a str> {
        let prefix = format!("{}{}", self.namespace, self.separator);
        name.strip_prefix(&prefix)
    }

    /// Prefix a resource URI with the namespace.
    pub fn prefixed_uri(&self, uri: &str) -> String {
        format!("{}{}{}", self.namespace, self.separator, uri)
    }

    /// Strip the namespace prefix from a URI.
    pub fn strip_uri_prefix<'a>(&self, uri: &'a str) -> Option<&'a str> {
        let prefix = format!("{}{}", self.namespace, self.separator);
        uri.strip_prefix(&prefix)
    }

    /// Get namespaced tool definitions from cache.
    pub async fn namespaced_tools(&self) -> Vec<ToolDefinition> {
        let cache = self.cache.read().await;
        cache
            .tools
            .iter()
            .map(|t| {
                let mut def = t.clone();
                def.name = self.prefixed_name(&def.name);
                def
            })
            .collect()
    }

    /// Get namespaced resource definitions from cache.
    pub async fn namespaced_resources(&self) -> Vec<ResourceDefinition> {
        let cache = self.cache.read().await;
        cache
            .resources
            .iter()
            .map(|r| {
                let mut def = r.clone();
                def.uri = self.prefixed_uri(&def.uri);
                def.name = self.prefixed_name(&def.name);
                def
            })
            .collect()
    }

    /// Get namespaced resource template definitions from cache.
    pub async fn namespaced_resource_templates(&self) -> Vec<ResourceTemplateDefinition> {
        let cache = self.cache.read().await;
        cache
            .resource_templates
            .iter()
            .map(|rt| {
                let mut def = rt.clone();
                def.uri_template = self.prefixed_uri(&def.uri_template);
                def.name = self.prefixed_name(&def.name);
                def
            })
            .collect()
    }

    /// Get namespaced prompt definitions from cache.
    pub async fn namespaced_prompts(&self) -> Vec<PromptDefinition> {
        let cache = self.cache.read().await;
        cache
            .prompts
            .iter()
            .map(|p| {
                let mut def = p.clone();
                def.name = self.prefixed_name(&def.name);
                def
            })
            .collect()
    }

    /// Call a tool on this backend (name should already have prefix stripped).
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult> {
        self.client.call_tool(name, arguments).await
    }

    /// Read a resource from this backend (uri should already have prefix stripped).
    pub async fn read_resource(&self, uri: &str) -> Result<ReadResourceResult> {
        self.client.read_resource(uri).await
    }

    /// Get a prompt from this backend (name should already have prefix stripped).
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: std::collections::HashMap<String, String>,
    ) -> Result<GetPromptResult> {
        let args = if arguments.is_empty() {
            None
        } else {
            Some(arguments)
        };
        self.client.get_prompt(name, args).await
    }
}
