//! Core proxy service implementing `Service<RouterRequest>`.

use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::sync::{RwLock, mpsc};
use tower::ServiceExt;
use tower::util::BoxCloneService;
use tower_service::Service;

use crate::client::ClientTransport;
use crate::protocol::{
    CallToolParams, GetPromptParams, Implementation, InitializeResult, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, McpRequest, McpResponse,
    PromptDefinition, ReadResourceParams, RequestId, ResourceDefinition,
    ResourceTemplateDefinition, ServerCapabilities, ToolDefinition, ToolsCapability,
};
use crate::router::{Extensions, RouterRequest, RouterResponse};
use crate::transport::CatchError;
use tower_mcp_types::JsonRpcError;

use super::backend::{Backend, BackendService, CachedCapabilities, ListChanged};

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
///
/// Backends can be added dynamically at runtime via [`add_backend()`](Self::add_backend).
/// All clones of the proxy share the same backend list, so additions are
/// immediately visible to all request handlers.
#[derive(Clone)]
pub struct McpProxy {
    pub(super) shared: Arc<McpProxyShared>,
    /// Per-backend entries with type-erased services. Shared across all clones
    /// via `Arc<Mutex<_>>`. The mutex is only held during synchronous routing
    /// and cloning operations (microseconds), never across await points.
    pub(super) entries: Arc<Mutex<Vec<BackendEntry>>>,
}

/// Shared, `Send + Sync` state for the proxy (no `BoxCloneService`).
pub(super) struct McpProxyShared {
    name: String,
    version: String,
    pub(super) backends: RwLock<Vec<Backend>>,
    /// Optional sender for forwarding list-changed notifications downstream.
    pub(super) notification_tx: Option<crate::context::NotificationSender>,
    /// Aggregated or custom instructions for the initialize response.
    instructions: Option<String>,
    /// Separator used for namespace prefixing.
    separator: String,
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

/// Error returned when adding a dynamic backend fails.
#[derive(Debug)]
pub enum AddBackendError {
    /// The namespace is already in use by an existing backend.
    DuplicateNamespace(String),
    /// The namespace would create an ambiguous prefix with an existing backend.
    AmbiguousPrefix {
        new_namespace: String,
        existing_namespace: String,
    },
    /// Failed to connect the transport.
    Connect(crate::error::Error),
    /// Failed during MCP initialization handshake.
    Initialize(crate::error::Error),
}

impl fmt::Display for AddBackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateNamespace(ns) => write!(f, "namespace \"{}\" already exists", ns),
            Self::AmbiguousPrefix {
                new_namespace,
                existing_namespace,
            } => write!(
                f,
                "namespace \"{}\" creates ambiguous prefix with \"{}\"",
                new_namespace, existing_namespace
            ),
            Self::Connect(e) => write!(f, "failed to connect: {}", e),
            Self::Initialize(e) => write!(f, "failed to initialize: {}", e),
        }
    }
}

impl std::error::Error for AddBackendError {}

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
        instructions: Option<String>,
        separator: String,
    ) -> Self {
        Self {
            shared: Arc::new(McpProxyShared {
                name,
                version,
                backends: RwLock::new(backends),
                notification_tx,
                instructions,
                separator,
            }),
            entries: Arc::new(Mutex::new(entries)),
        }
    }

    /// Add a backend dynamically from a [`ClientTransport`].
    ///
    /// The transport is connected, the MCP initialize handshake runs, and
    /// capabilities are discovered. The new backend is immediately available
    /// to all clones of this proxy.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The namespace is already in use
    /// - The namespace creates an ambiguous prefix with an existing backend
    /// - The transport fails to connect
    /// - The MCP initialize handshake fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// proxy.add_backend("new-db", StdioClientTransport::spawn("db-server", &[]).await?).await?;
    /// // New backend is immediately available for requests
    /// ```
    pub async fn add_backend(
        &self,
        namespace: impl Into<String>,
        transport: impl ClientTransport,
    ) -> Result<(), AddBackendError> {
        let namespace = namespace.into();
        let separator = self.shared.separator.clone();

        // Validate namespace uniqueness and prefix ambiguity
        {
            let entries = self.entries.lock().expect("entries lock poisoned");
            self.validate_namespace(&namespace, &separator, &entries)?;
        }

        // Connect and initialize (no lock held during async operations)
        let (invalidation_tx, invalidation_rx) = mpsc::channel(16);
        let backend = Backend::connect(namespace.clone(), transport, separator, invalidation_tx)
            .await
            .map_err(AddBackendError::Connect)?;

        backend
            .initialize(&self.shared.name, &self.shared.version)
            .await
            .map_err(AddBackendError::Initialize)?;

        let entry = BackendEntry::from_backend(&backend);

        // Add to shared state
        {
            let mut entries = self.entries.lock().expect("entries lock poisoned");
            entries.push(entry);
        }

        // Spawn invalidation watcher
        let backend_idx = {
            let mut backends = self.shared.backends.write().await;
            let idx = backends.len();
            backends.push(backend);
            idx
        };

        self.spawn_invalidation_watcher(backend_idx, invalidation_rx);

        // Notify downstream clients that capabilities changed
        self.notify_all_changed().await;

        Ok(())
    }

    /// Add a backend dynamically with a custom middleware-wrapped service.
    ///
    /// Like [`add_backend()`](Self::add_backend), but applies a Tower layer
    /// to the backend's dispatch service before adding it.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use std::time::Duration;
    /// use tower::timeout::TimeoutLayer;
    ///
    /// proxy.add_backend_with_layer(
    ///     "slow-api",
    ///     transport,
    ///     TimeoutLayer::new(Duration::from_secs(60)),
    /// ).await?;
    /// ```
    pub async fn add_backend_with_layer<L>(
        &self,
        namespace: impl Into<String>,
        transport: impl ClientTransport,
        layer: L,
    ) -> Result<(), AddBackendError>
    where
        L: tower::Layer<BackendService> + Send + 'static,
        L::Service: Service<RouterRequest, Response = RouterResponse> + Clone + Send + 'static,
        <L::Service as Service<RouterRequest>>::Error: fmt::Display + Send,
        <L::Service as Service<RouterRequest>>::Future: Send,
    {
        let namespace = namespace.into();
        let separator = self.shared.separator.clone();

        {
            let entries = self.entries.lock().expect("entries lock poisoned");
            self.validate_namespace(&namespace, &separator, &entries)?;
        }

        let (invalidation_tx, invalidation_rx) = mpsc::channel(16);
        let backend = Backend::connect(namespace.clone(), transport, separator, invalidation_tx)
            .await
            .map_err(AddBackendError::Connect)?;

        backend
            .initialize(&self.shared.name, &self.shared.version)
            .await
            .map_err(AddBackendError::Initialize)?;

        // Apply middleware layer
        let base = backend.service();
        let layered = layer.layer(base);
        let caught = CatchError::new(layered);
        let service = BoxCloneService::new(caught);
        let entry = BackendEntry::from_backend_with_service(&backend, service);

        {
            let mut entries = self.entries.lock().expect("entries lock poisoned");
            entries.push(entry);
        }

        let backend_idx = {
            let mut backends = self.shared.backends.write().await;
            let idx = backends.len();
            backends.push(backend);
            idx
        };

        self.spawn_invalidation_watcher(backend_idx, invalidation_rx);

        if let Some(tx) = &self.shared.notification_tx {
            let _ = tx
                .send(crate::context::ServerNotification::ToolsListChanged)
                .await;
            let _ = tx
                .send(crate::context::ServerNotification::ResourcesListChanged)
                .await;
            let _ = tx
                .send(crate::context::ServerNotification::PromptsListChanged)
                .await;
        }

        Ok(())
    }

    /// Validate that a namespace can be added without conflicts.
    fn validate_namespace(
        &self,
        namespace: &str,
        separator: &str,
        entries: &[BackendEntry],
    ) -> Result<(), AddBackendError> {
        let new_prefix = format!("{}{}", namespace, separator);

        for entry in entries {
            if entry.namespace == namespace {
                return Err(AddBackendError::DuplicateNamespace(namespace.to_string()));
            }
            let existing_prefix = format!("{}{}", entry.namespace, entry.separator);
            if new_prefix.starts_with(&existing_prefix) || existing_prefix.starts_with(&new_prefix)
            {
                return Err(AddBackendError::AmbiguousPrefix {
                    new_namespace: namespace.to_string(),
                    existing_namespace: entry.namespace.clone(),
                });
            }
        }
        Ok(())
    }

    /// Spawn a background task that watches for list-changed notifications
    /// from a backend and refreshes the cache.
    pub(super) fn spawn_invalidation_watcher(
        &self,
        backend_idx: usize,
        mut rx: mpsc::Receiver<ListChanged>,
    ) {
        let shared = Arc::clone(&self.shared);
        tokio::spawn(async move {
            while let Some(changed) = rx.recv().await {
                let backends = shared.backends.read().await;
                let Some(backend) = backends.get(backend_idx) else {
                    break;
                };
                tracing::debug!(
                    namespace = %backend.namespace,
                    kind = ?changed,
                    "Backend list changed, refreshing cache"
                );
                match changed {
                    ListChanged::Tools => {
                        backend.refresh_tools().await;
                        if let Some(tx) = &shared.notification_tx {
                            let _ = tx
                                .send(crate::context::ServerNotification::ToolsListChanged)
                                .await;
                        }
                    }
                    ListChanged::Resources => {
                        backend.refresh_resources().await;
                        if let Some(tx) = &shared.notification_tx {
                            let _ = tx
                                .send(crate::context::ServerNotification::ResourcesListChanged)
                                .await;
                        }
                    }
                    ListChanged::Prompts => {
                        backend.refresh_prompts().await;
                        if let Some(tx) = &shared.notification_tx {
                            let _ = tx
                                .send(crate::context::ServerNotification::PromptsListChanged)
                                .await;
                        }
                    }
                }
            }
        });
    }

    /// Remove a backend by namespace name.
    ///
    /// The backend's tools, resources, and prompts are immediately removed from
    /// aggregated lists. Downstream clients are notified via list-changed
    /// notifications.
    ///
    /// Returns `true` if the backend was found and removed, `false` if no
    /// backend with that namespace exists.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if proxy.remove_backend("old-db").await {
    ///     println!("Backend removed");
    /// }
    /// ```
    pub async fn remove_backend(&self, namespace: &str) -> bool {
        // Remove from entries (synchronous)
        let found = {
            let mut entries = self.entries.lock().expect("entries lock poisoned");
            let before = entries.len();
            entries.retain(|e| e.namespace != namespace);
            entries.len() < before
        };

        if !found {
            return false;
        }

        // Remove from backends (async)
        // Dropping the Backend drops its invalidation_tx sender, which
        // causes the spawned invalidation watcher to exit naturally.
        {
            let mut backends = self.shared.backends.write().await;
            backends.retain(|b| b.namespace != namespace);
        }

        // Notify downstream clients that capabilities changed
        self.notify_all_changed().await;

        tracing::info!(namespace, "Backend removed");
        true
    }

    /// Replace a backend by removing the old one and adding a new one with
    /// the same namespace.
    ///
    /// This is a convenience for `remove_backend()` + `add_backend()`. If
    /// the add fails, the old backend is already gone (no rollback).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// proxy.replace_backend("db", new_transport).await?;
    /// ```
    pub async fn replace_backend(
        &self,
        namespace: impl Into<String>,
        transport: impl ClientTransport,
    ) -> Result<(), AddBackendError> {
        let namespace = namespace.into();
        self.remove_backend(&namespace).await;
        self.add_backend(namespace, transport).await
    }

    /// Return the namespaces of all currently registered backends.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let namespaces = proxy.backend_namespaces();
    /// println!("Backends: {:?}", namespaces);
    /// ```
    pub fn backend_namespaces(&self) -> Vec<String> {
        let entries = self.entries.lock().expect("entries lock poisoned");
        entries.iter().map(|e| e.namespace.clone()).collect()
    }

    /// Return the number of currently registered backends.
    pub fn backend_count(&self) -> usize {
        self.entries.lock().expect("entries lock poisoned").len()
    }

    /// Check the health of all backends by pinging them concurrently.
    ///
    /// Returns a map of namespace to health status. Backends that respond
    /// to ping within a reasonable time are considered healthy.
    pub async fn health_check(&self) -> Vec<BackendHealth> {
        let backends = self.shared.backends.read().await;
        let futures: Vec<_> = backends
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
        drop(backends);

        futures::future::join_all(futures).await
    }

    /// Notify downstream clients that all capability lists have changed.
    async fn notify_all_changed(&self) {
        if let Some(tx) = &self.shared.notification_tx {
            let _ = tx
                .send(crate::context::ServerNotification::ToolsListChanged)
                .await;
            let _ = tx
                .send(crate::context::ServerNotification::ResourcesListChanged)
                .await;
            let _ = tx
                .send(crate::context::ServerNotification::PromptsListChanged)
                .await;
        }
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
    instructions: Option<String>,
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
        instructions,
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
    service: BoxCloneService<RouterRequest, RouterResponse, Infallible>,
    stripped_name: String,
    params: CallToolParams,
    id: RequestId,
    extensions: Extensions,
) -> Result<McpResponse, JsonRpcError> {
    let inner_request = McpRequest::CallTool(CallToolParams {
        name: stripped_name,
        arguments: params.arguments,
        meta: params.meta,
        task: params.task,
    });

    let router_req = RouterRequest {
        id,
        inner: inner_request,
        extensions,
    };

    let resp = service.oneshot(router_req).await.expect("infallible");
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
    service: BoxCloneService<RouterRequest, RouterResponse, Infallible>,
    stripped_uri: String,
    params: ReadResourceParams,
    id: RequestId,
    extensions: Extensions,
) -> Result<McpResponse, JsonRpcError> {
    let inner_request = McpRequest::ReadResource(ReadResourceParams {
        uri: stripped_uri,
        meta: params.meta,
    });

    let router_req = RouterRequest {
        id,
        inner: inner_request,
        extensions,
    };

    let resp = service.oneshot(router_req).await.expect("infallible");
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
    service: BoxCloneService<RouterRequest, RouterResponse, Infallible>,
    stripped_name: String,
    params: GetPromptParams,
    id: RequestId,
    extensions: Extensions,
) -> Result<McpResponse, JsonRpcError> {
    let inner_request = McpRequest::GetPrompt(GetPromptParams {
        name: stripped_name,
        arguments: params.arguments,
        meta: params.meta,
    });

    let router_req = RouterRequest {
        id,
        inner: inner_request,
        extensions,
    };

    let resp = service.oneshot(router_req).await.expect("infallible");
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
        let extensions = req.extensions.clone();

        // Lock entries for synchronous routing and data extraction.
        // The lock is held only for cloning services and extracting EntryInfo —
        // no await points occur while the lock is held.
        let entries = self.entries.lock().expect("entries lock poisoned");

        let result_future: Pin<Box<dyn Future<Output = Result<McpResponse, JsonRpcError>> + Send>> =
            match req.inner {
                McpRequest::Initialize(_params) => {
                    let name = self.shared.name.clone();
                    let version = self.shared.version.clone();
                    let instructions = self.shared.instructions.clone();
                    Box::pin(handle_initialize(name, version, instructions))
                }
                McpRequest::Ping => Box::pin(async { Ok(McpResponse::Pong(Default::default())) }),
                McpRequest::ListTools(_params) => {
                    let infos = Self::entry_infos(&entries);
                    Box::pin(handle_list_tools(infos))
                }
                McpRequest::CallTool(params) => {
                    match Self::route_by_prefix(&entries, &params.name) {
                        Some((idx, stripped)) => {
                            let service = entries[idx].service.clone();
                            Box::pin(handle_call_tool(
                                service,
                                stripped,
                                params,
                                request_id.clone(),
                                extensions.clone(),
                            ))
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
                    let infos = Self::entry_infos(&entries);
                    Box::pin(handle_list_resources(infos))
                }
                McpRequest::ListResourceTemplates(_params) => {
                    let infos = Self::entry_infos(&entries);
                    Box::pin(handle_list_resource_templates(infos))
                }
                McpRequest::ReadResource(params) => {
                    match Self::route_by_uri_prefix(&entries, &params.uri) {
                        Some((idx, stripped)) => {
                            let service = entries[idx].service.clone();
                            Box::pin(handle_read_resource(
                                service,
                                stripped,
                                params,
                                request_id.clone(),
                                extensions.clone(),
                            ))
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
                    let infos = Self::entry_infos(&entries);
                    Box::pin(handle_list_prompts(infos))
                }
                McpRequest::GetPrompt(params) => {
                    match Self::route_by_prefix(&entries, &params.name) {
                        Some((idx, stripped)) => {
                            let service = entries[idx].service.clone();
                            Box::pin(handle_get_prompt(
                                service,
                                stripped,
                                params,
                                request_id.clone(),
                                extensions.clone(),
                            ))
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

        // Drop the lock before returning the future
        drop(entries);

        Box::pin(async move {
            let result = result_future.await;
            Ok(RouterResponse {
                id: request_id,
                inner: result,
            })
        })
    }
}
