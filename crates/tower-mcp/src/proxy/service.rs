//! Core proxy service implementing `Service<RouterRequest>`.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::sync::RwLock;
use tower::util::BoxCloneService;
use tower_service::Service;

use crate::protocol::{
    CallToolParams, GetPromptParams, Implementation, InitializeResult, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, McpRequest, McpResponse,
    PromptDefinition, ReadResourceParams, RequestId, ResourceDefinition,
    ResourceTemplateDefinition, ServerCapabilities, ToolDefinition, ToolsCapability,
};
use crate::router::{Extensions, RouterRequest, RouterResponse};
use tower_mcp_types::JsonRpcError;

use super::backend::{Backend, CachedCapabilities};

/// A backend entry in the proxy, combining cached capabilities with a
/// type-erased service for dispatching routed requests.
#[derive(Clone)]
pub(crate) struct BackendEntry {
    pub namespace: String,
    pub separator: String,
    pub cache: Arc<RwLock<CachedCapabilities>>,
    /// Type-erased service for dispatching call_tool, read_resource, get_prompt.
    /// Middleware layers are applied before type-erasure.
    pub service: BoxCloneService<RouterRequest, RouterResponse, Infallible>,
}

impl BackendEntry {
    /// Create from a Backend using its default BackendService (no middleware).
    pub fn from_backend(backend: &Backend) -> Self {
        Self {
            namespace: backend.namespace.clone(),
            separator: backend.separator.clone(),
            cache: Arc::clone(&backend.cache),
            service: BoxCloneService::new(backend.service()),
        }
    }

    /// Create from a Backend with a custom (already-layered) service.
    pub fn from_backend_with_service(
        backend: &Backend,
        service: BoxCloneService<RouterRequest, RouterResponse, Infallible>,
    ) -> Self {
        Self {
            namespace: backend.namespace.clone(),
            separator: backend.separator.clone(),
            cache: Arc::clone(&backend.cache),
            service,
        }
    }

    /// Strip the namespace prefix from a name, if it matches.
    fn strip_prefix<'a>(&self, name: &'a str) -> Option<&'a str> {
        let prefix = format!("{}{}", self.namespace, self.separator);
        name.strip_prefix(&prefix)
    }

    /// Strip the namespace prefix from a URI.
    fn strip_uri_prefix<'a>(&self, uri: &'a str) -> Option<&'a str> {
        let prefix = format!("{}{}", self.namespace, self.separator);
        uri.strip_prefix(&prefix)
    }
}

/// An MCP proxy that aggregates multiple backend servers.
///
/// Implements `Service<RouterRequest>` so it can be used with any tower-mcp
/// transport (HTTP, WebSocket, stdio) and composed with tower middleware.
///
/// Each backend's capabilities are namespaced to avoid collisions, and
/// individual backends can have their own Tower middleware stack applied
/// via [`McpProxyBuilder::backend_layer()`](super::McpProxyBuilder::backend_layer).
#[derive(Clone)]
pub struct McpProxy {
    pub(super) shared: Arc<McpProxyShared>,
    /// Per-backend entries with type-erased services. Kept outside `Arc`
    /// because `BoxCloneService` is `Send + Clone` but not `Sync`.
    pub(super) entries: Vec<BackendEntry>,
}

/// Shared, `Send + Sync` state for the proxy (no `BoxCloneService`).
pub(super) struct McpProxyShared {
    name: String,
    version: String,
    pub(super) backends: Vec<Backend>,
    /// Optional sender for forwarding list-changed notifications downstream.
    pub(super) notification_tx: Option<crate::context::NotificationSender>,
}

/// Health status of a single backend.
#[derive(Debug, Clone)]
pub struct BackendHealth {
    /// The backend's namespace.
    pub namespace: String,
    /// Whether the backend responded to a ping.
    pub healthy: bool,
}

/// Namespace + cache extracted from a `BackendEntry` for `Send`-safe async use.
/// Both fields are `Send + Sync`, so futures holding these can cross thread boundaries.
struct EntryInfo {
    namespace: String,
    separator: String,
    cache: Arc<RwLock<CachedCapabilities>>,
}

impl McpProxy {
    /// Create a builder for configuring the proxy.
    pub fn builder(name: impl Into<String>, version: impl Into<String>) -> super::McpProxyBuilder {
        super::McpProxyBuilder::new(name, version)
    }

    /// Create a new proxy with the given backends and entries (called by builder).
    pub(crate) fn new(
        name: String,
        version: String,
        backends: Vec<Backend>,
        entries: Vec<BackendEntry>,
        notification_tx: Option<crate::context::NotificationSender>,
    ) -> Self {
        Self {
            shared: Arc::new(McpProxyShared {
                name,
                version,
                backends,
                notification_tx,
            }),
            entries,
        }
    }

    /// Check the health of all backends by pinging them concurrently.
    ///
    /// Returns a map of namespace to health status. Backends that respond
    /// to ping within a reasonable time are considered healthy.
    pub async fn health_check(&self) -> Vec<BackendHealth> {
        let futures: Vec<_> = self
            .shared
            .backends
            .iter()
            .map(|backend| {
                let client = Arc::clone(&backend.client);
                let namespace = backend.namespace.clone();
                async move {
                    let healthy = client.ping().await.is_ok();
                    BackendHealth { namespace, healthy }
                }
            })
            .collect();

        futures::future::join_all(futures).await
    }

    /// Extract Send-safe info from entries (synchronous, no borrow across await).
    fn entry_infos(entries: &[BackendEntry]) -> Vec<EntryInfo> {
        entries
            .iter()
            .map(|e| EntryInfo {
                namespace: e.namespace.clone(),
                separator: e.separator.clone(),
                cache: Arc::clone(&e.cache),
            })
            .collect()
    }

    /// Route a namespaced name to the correct backend index + stripped name.
    fn route_by_prefix(entries: &[BackendEntry], name: &str) -> Option<(usize, String)> {
        for (i, entry) in entries.iter().enumerate() {
            if let Some(stripped) = entry.strip_prefix(name) {
                return Some((i, stripped.to_string()));
            }
        }
        None
    }

    /// Route a namespaced URI to the correct backend index + stripped URI.
    fn route_by_uri_prefix(entries: &[BackendEntry], uri: &str) -> Option<(usize, String)> {
        for (i, entry) in entries.iter().enumerate() {
            if let Some(stripped) = entry.strip_uri_prefix(uri) {
                return Some((i, stripped.to_string()));
            }
        }
        None
    }
}

impl EntryInfo {
    fn prefixed_name(&self, name: &str) -> String {
        format!("{}{}{}", self.namespace, self.separator, name)
    }

    fn prefixed_uri(&self, uri: &str) -> String {
        format!("{}{}{}", self.namespace, self.separator, uri)
    }
}

// =========================================================================
// Async handlers that only touch Send+Sync data (no BackendEntry references
// held across .await points).
// =========================================================================

async fn handle_initialize(
    name: String,
    version: String,
    num_backends: usize,
) -> Result<McpResponse, JsonRpcError> {
    Ok(McpResponse::Initialize(InitializeResult {
        protocol_version: "2025-11-25".to_string(),
        server_info: Implementation {
            name,
            version,
            title: None,
            description: None,
            icons: None,
            website_url: None,
            meta: None,
        },
        capabilities: ServerCapabilities {
            tools: Some(ToolsCapability { list_changed: true }),
            resources: None,
            prompts: None,
            logging: None,
            tasks: None,
            completions: None,
            experimental: None,
            extensions: None,
        },
        instructions: Some(format!(
            "MCP proxy aggregating {} backend servers",
            num_backends
        )),
        meta: None,
    }))
}

async fn handle_list_tools(infos: Vec<EntryInfo>) -> Result<McpResponse, JsonRpcError> {
    let mut tools = Vec::new();
    for info in &infos {
        let cache = info.cache.read().await;
        for t in &cache.tools {
            let mut def: ToolDefinition = t.clone();
            def.name = info.prefixed_name(&def.name);
            tools.push(def);
        }
    }
    Ok(McpResponse::ListTools(ListToolsResult {
        tools,
        next_cursor: None,
        meta: None,
    }))
}

async fn handle_call_tool(
    mut service: BoxCloneService<RouterRequest, RouterResponse, Infallible>,
    stripped_name: String,
    params: CallToolParams,
) -> Result<McpResponse, JsonRpcError> {
    let inner_request = McpRequest::CallTool(CallToolParams {
        name: stripped_name,
        arguments: params.arguments,
        meta: params.meta,
        task: params.task,
    });

    let router_req = RouterRequest {
        id: RequestId::Number(0),
        inner: inner_request,
        extensions: Extensions::new(),
    };

    let resp = service.call(router_req).await.expect("infallible");
    resp.inner
}

async fn handle_list_resources(infos: Vec<EntryInfo>) -> Result<McpResponse, JsonRpcError> {
    let mut resources = Vec::new();
    for info in &infos {
        let cache = info.cache.read().await;
        for r in &cache.resources {
            let mut def: ResourceDefinition = r.clone();
            def.uri = info.prefixed_uri(&def.uri);
            def.name = info.prefixed_name(&def.name);
            resources.push(def);
        }
    }
    Ok(McpResponse::ListResources(ListResourcesResult {
        resources,
        next_cursor: None,
        meta: None,
    }))
}

async fn handle_list_resource_templates(
    infos: Vec<EntryInfo>,
) -> Result<McpResponse, JsonRpcError> {
    let mut resource_templates = Vec::new();
    for info in &infos {
        let cache = info.cache.read().await;
        for rt in &cache.resource_templates {
            let mut def: ResourceTemplateDefinition = rt.clone();
            def.uri_template = info.prefixed_uri(&def.uri_template);
            def.name = info.prefixed_name(&def.name);
            resource_templates.push(def);
        }
    }
    Ok(McpResponse::ListResourceTemplates(
        ListResourceTemplatesResult {
            resource_templates,
            next_cursor: None,
            meta: None,
        },
    ))
}

async fn handle_read_resource(
    mut service: BoxCloneService<RouterRequest, RouterResponse, Infallible>,
    stripped_uri: String,
    params: ReadResourceParams,
) -> Result<McpResponse, JsonRpcError> {
    let inner_request = McpRequest::ReadResource(ReadResourceParams {
        uri: stripped_uri,
        meta: params.meta,
    });

    let router_req = RouterRequest {
        id: RequestId::Number(0),
        inner: inner_request,
        extensions: Extensions::new(),
    };

    let resp = service.call(router_req).await.expect("infallible");
    resp.inner
}

async fn handle_list_prompts(infos: Vec<EntryInfo>) -> Result<McpResponse, JsonRpcError> {
    let mut prompts = Vec::new();
    for info in &infos {
        let cache = info.cache.read().await;
        for p in &cache.prompts {
            let mut def: PromptDefinition = p.clone();
            def.name = info.prefixed_name(&def.name);
            prompts.push(def);
        }
    }
    Ok(McpResponse::ListPrompts(ListPromptsResult {
        prompts,
        next_cursor: None,
        meta: None,
    }))
}

async fn handle_get_prompt(
    mut service: BoxCloneService<RouterRequest, RouterResponse, Infallible>,
    stripped_name: String,
    params: GetPromptParams,
) -> Result<McpResponse, JsonRpcError> {
    let inner_request = McpRequest::GetPrompt(GetPromptParams {
        name: stripped_name,
        arguments: params.arguments,
        meta: params.meta,
    });

    let router_req = RouterRequest {
        id: RequestId::Number(0),
        inner: inner_request,
        extensions: Extensions::new(),
    };

    let resp = service.call(router_req).await.expect("infallible");
    resp.inner
}

// =========================================================================
// Service implementation
// =========================================================================

impl Service<RouterRequest> for McpProxy {
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let request_id = req.id.clone();

        // All routing and data extraction happens synchronously here (no await),
        // so we never hold a &[BackendEntry] reference across an await point.
        // Only Send+Sync data (EntryInfo, cloned BoxCloneService, Arc<McpProxyShared>)
        // is moved into the returned future.
        let result_future: Pin<Box<dyn Future<Output = Result<McpResponse, JsonRpcError>> + Send>> =
            match req.inner {
                McpRequest::Initialize(_params) => {
                    let name = self.shared.name.clone();
                    let version = self.shared.version.clone();
                    let num = self.entries.len();
                    Box::pin(handle_initialize(name, version, num))
                }
                McpRequest::Ping => Box::pin(async { Ok(McpResponse::Pong(Default::default())) }),
                McpRequest::ListTools(_params) => {
                    let infos = Self::entry_infos(&self.entries);
                    Box::pin(handle_list_tools(infos))
                }
                McpRequest::CallTool(params) => {
                    match Self::route_by_prefix(&self.entries, &params.name) {
                        Some((idx, stripped)) => {
                            let service = self.entries[idx].service.clone();
                            Box::pin(handle_call_tool(service, stripped, params))
                        }
                        None => Box::pin(async move {
                            Err(JsonRpcError::invalid_params(format!(
                                "Unknown tool: {}",
                                params.name
                            )))
                        }),
                    }
                }
                McpRequest::ListResources(_params) => {
                    let infos = Self::entry_infos(&self.entries);
                    Box::pin(handle_list_resources(infos))
                }
                McpRequest::ListResourceTemplates(_params) => {
                    let infos = Self::entry_infos(&self.entries);
                    Box::pin(handle_list_resource_templates(infos))
                }
                McpRequest::ReadResource(params) => {
                    match Self::route_by_uri_prefix(&self.entries, &params.uri) {
                        Some((idx, stripped)) => {
                            let service = self.entries[idx].service.clone();
                            Box::pin(handle_read_resource(service, stripped, params))
                        }
                        None => Box::pin(async move {
                            Err(JsonRpcError::invalid_params(format!(
                                "Unknown resource: {}",
                                params.uri
                            )))
                        }),
                    }
                }
                McpRequest::ListPrompts(_params) => {
                    let infos = Self::entry_infos(&self.entries);
                    Box::pin(handle_list_prompts(infos))
                }
                McpRequest::GetPrompt(params) => {
                    match Self::route_by_prefix(&self.entries, &params.name) {
                        Some((idx, stripped)) => {
                            let service = self.entries[idx].service.clone();
                            Box::pin(handle_get_prompt(service, stripped, params))
                        }
                        None => Box::pin(async move {
                            Err(JsonRpcError::invalid_params(format!(
                                "Unknown prompt: {}",
                                params.name
                            )))
                        }),
                    }
                }
                _ => Box::pin(async {
                    Err(JsonRpcError::method_not_found(
                        "Method not supported by proxy",
                    ))
                }),
            };

        Box::pin(async move {
            let result = result_future.await;
            Ok(RouterResponse {
                id: request_id,
                inner: result,
            })
        })
    }
}
