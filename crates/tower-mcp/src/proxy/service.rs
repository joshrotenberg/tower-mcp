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
        /// Namespace requested for the backend being added.
        new_namespace: String,
        /// Existing namespace whose prefix would conflict.
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

/// A backend that is connected and initialized but not yet published.
struct StagedBackend {
    backend: Backend,
    invalidation_rx: mpsc::Receiver<ListChanged>,
}

/// What a commit does with an existing backend of the same namespace.
#[derive(Clone, Copy)]
enum CommitMode {
    /// The namespace must be free.
    Add,
    /// An existing backend of the namespace is replaced in place.
    Replace,
}

/// The routing entry for `backend` with `layer` applied to its service.
fn layered_entry<L>(backend: &Backend, layer: L) -> BackendEntry
where
    L: tower::Layer<BackendService> + Send + 'static,
    L::Service: Service<RouterRequest, Response = RouterResponse> + Clone + Send + 'static,
    <L::Service as Service<RouterRequest>>::Error: fmt::Display + Send,
    <L::Service as Service<RouterRequest>>::Future: Send,
{
    let caught = CatchError::new(layer.layer(backend.service()));
    BackendEntry::from_backend_with_service(backend, BoxCloneService::new(caught))
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
    /// capabilities are discovered before the backend is published. The new
    /// backend is then visible to all clones of this proxy at once.
    ///
    /// Membership changes ([`add_backend`](Self::add_backend),
    /// [`remove_backend`](Self::remove_backend),
    /// [`replace_backend`](Self::replace_backend) and their `_with_layer`
    /// forms) are serialized at a single commit point and take effect in the
    /// order they commit. The namespace is checked before connecting and again
    /// at commit, since another call may have claimed it in the meantime.
    /// Dropping the returned future before it completes publishes nothing.
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
        self.check_namespace(&namespace, None)?;
        let staged = self.stage(namespace, transport).await?;
        let entry = BackendEntry::from_backend(&staged.backend);
        self.commit(staged, entry, CommitMode::Add).await
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
        self.check_namespace(&namespace, None)?;
        let staged = self.stage(namespace, transport).await?;
        let entry = layered_entry(&staged.backend, layer);
        self.commit(staged, entry, CommitMode::Add).await
    }

    /// Validate that a namespace can be added without conflicts.
    ///
    /// `replacing` names a namespace whose existing backend is about to be
    /// replaced, so it does not count as a conflict with itself.
    fn validate_namespace(
        &self,
        namespace: &str,
        separator: &str,
        entries: &[BackendEntry],
        replacing: Option<&str>,
    ) -> Result<(), AddBackendError> {
        let new_prefix = format!("{}{}", namespace, separator);

        for entry in entries {
            if replacing == Some(entry.namespace.as_str()) {
                continue;
            }
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

    /// Early namespace check, so a doomed add fails before connecting.
    fn check_namespace(
        &self,
        namespace: &str,
        replacing: Option<&str>,
    ) -> Result<(), AddBackendError> {
        let entries = self.entries.lock().expect("entries lock poisoned");
        self.validate_namespace(namespace, &self.shared.separator, &entries, replacing)
    }

    /// Connect, initialize, and discover a backend without publishing it.
    ///
    /// Nothing shared is touched here, so a failure or a dropped future
    /// leaves the proxy exactly as it was.
    async fn stage(
        &self,
        namespace: String,
        transport: impl ClientTransport,
    ) -> Result<StagedBackend, AddBackendError> {
        let (invalidation_tx, invalidation_rx) = mpsc::channel(16);
        let backend = Backend::connect(
            namespace,
            transport,
            self.shared.separator.clone(),
            invalidation_tx,
        )
        .await
        .map_err(AddBackendError::Connect)?;

        backend
            .initialize(&self.shared.name, &self.shared.version)
            .await
            .map_err(AddBackendError::Initialize)?;

        Ok(StagedBackend {
            backend,
            invalidation_rx,
        })
    }

    /// Publish a staged backend.
    ///
    /// This is the single commit point for membership changes. The
    /// `backends` write lock is taken first and is the only `.await` before
    /// both lists change, and both change with no `.await` between them, so
    /// commits are serialized, a dropped future either changed nothing or
    /// changed both lists, and routing never sees one list ahead of the other.
    async fn commit(
        &self,
        staged: StagedBackend,
        entry: BackendEntry,
        mode: CommitMode,
    ) -> Result<(), AddBackendError> {
        let StagedBackend {
            backend,
            invalidation_rx,
        } = staged;
        let backend_id = backend.id;
        let namespace = backend.namespace.clone();
        let separator = self.shared.separator.clone();

        // The replaced backend, if any, is dropped only after both locks are
        // released.
        let _replaced = {
            let mut backends = self.shared.backends.write().await;
            let mut entries = self.entries.lock().expect("entries lock poisoned");
            match mode {
                CommitMode::Add => {
                    self.validate_namespace(&namespace, &separator, &entries, None)?;
                    entries.push(entry);
                    backends.push(backend);
                    None
                }
                CommitMode::Replace => {
                    self.validate_namespace(&namespace, &separator, &entries, Some(&namespace))?;
                    match entries.iter().position(|e| e.namespace == namespace) {
                        Some(i) => entries[i] = entry,
                        None => entries.push(entry),
                    }
                    match backends.iter().position(|b| b.namespace == namespace) {
                        Some(i) => Some(std::mem::replace(&mut backends[i], backend)),
                        None => {
                            backends.push(backend);
                            None
                        }
                    }
                }
            }
        };

        self.spawn_invalidation_watcher(backend_id, invalidation_rx);
        self.notify_all_changed().await;
        Ok(())
    }

    /// Spawn a background task that watches for list-changed notifications
    /// from a backend and refreshes the cache.
    ///
    /// The watcher follows the backend by [`Backend::id`], not by position or
    /// namespace: once that backend is removed or replaced it exits rather
    /// than refreshing whichever backend took its place. The `backends` lock
    /// is released before any refresh call, so a slow backend cannot hold up
    /// a membership change.
    pub(super) fn spawn_invalidation_watcher(
        &self,
        backend_id: u64,
        mut rx: mpsc::Receiver<ListChanged>,
    ) {
        let shared = Arc::clone(&self.shared);
        tokio::spawn(async move {
            while let Some(changed) = rx.recv().await {
                let backend = {
                    let backends = shared.backends.read().await;
                    match backends.iter().find(|b| b.id == backend_id) {
                        Some(backend) => backend.clone(),
                        None => break,
                    }
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
        // Same commit point as `commit`: the write lock first, then both
        // lists with no `.await` in between. Dropping the removed Backend
        // drops its invalidation sender, which ends its watcher.
        let removed = {
            let mut backends = self.shared.backends.write().await;
            let mut entries = self.entries.lock().expect("entries lock poisoned");
            let before = entries.len();
            entries.retain(|e| e.namespace != namespace);
            let found = entries.len() < before;
            let mut removed = Vec::new();
            backends.retain(|b| {
                if b.namespace == namespace {
                    removed.push(b.clone());
                    false
                } else {
                    true
                }
            });
            found.then_some(removed)
        };

        if removed.is_none() {
            return false;
        }
        drop(removed);

        // Notify downstream clients that capabilities changed
        self.notify_all_changed().await;

        tracing::info!(namespace, "Backend removed");
        true
    }

    /// Replace a backend with a new connection under the same namespace.
    ///
    /// The replacement is connected, initialized, and has its capabilities
    /// discovered while the existing backend keeps serving requests. Only
    /// then are routing and backend state switched over together, keeping
    /// the backend's position. If connecting or initializing fails, or the
    /// returned future is dropped before it completes, the existing backend
    /// is left in place and keeps serving.
    ///
    /// Replacing a namespace that has no backend adds one. Concurrent
    /// membership changes take effect in commit order (see
    /// [`add_backend`](Self::add_backend)), so of two concurrent replaces of
    /// one namespace the later commit wins, and a replace that commits after
    /// a remove of the same namespace re-adds it.
    ///
    /// # Errors
    ///
    /// Returns an error if the namespace creates an ambiguous prefix with a
    /// different backend, or if the replacement fails to connect or
    /// initialize. In every error case the existing backend is unchanged.
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
        self.check_namespace(&namespace, Some(&namespace))?;
        let staged = self.stage(namespace, transport).await?;
        let entry = BackendEntry::from_backend(&staged.backend);
        self.commit(staged, entry, CommitMode::Replace).await
    }

    /// Replace a backend with a new middleware-wrapped connection.
    ///
    /// Like [`replace_backend()`](Self::replace_backend), with the same
    /// staging and commit guarantees, but applies a Tower layer to the
    /// replacement's dispatch service, as
    /// [`add_backend_with_layer()`](Self::add_backend_with_layer) does.
    pub async fn replace_backend_with_layer<L>(
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
        self.check_namespace(&namespace, Some(&namespace))?;
        let staged = self.stage(namespace, transport).await?;
        let entry = layered_entry(&staged.backend, layer);
        self.commit(staged, entry, CommitMode::Replace).await
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
            ..Default::default()
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
        ttl_ms: None,
        cache_scope: None,
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
        input_responses: params.input_responses,
        request_state: params.request_state,
        meta: params.meta,
        task: params.task,
    });

    let final_protocol = crate::router::is_final_protocol_request(&extensions);
    let router_req = RouterRequest {
        id,
        inner: inner_request,
        extensions,
    };

    let resp = service.oneshot(router_req).await.expect("infallible");
    let mut result = resp.inner;
    // A backend on 2026-07-28 may send an explicit `structuredContent: null`.
    // 2025-11-25 and earlier require an object when the field is present, so
    // a peer on those revisions gets the field omitted instead.
    if !final_protocol
        && let Ok(McpResponse::CallTool(call)) = &mut result
        && matches!(call.structured_content, Some(serde_json::Value::Null))
    {
        call.structured_content = None;
    }
    result
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
        ttl_ms: None,
        cache_scope: None,
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
            ttl_ms: None,
            cache_scope: None,
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
        input_responses: params.input_responses,
        request_state: params.request_state,
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
        ttl_ms: None,
        cache_scope: None,
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
        input_responses: params.input_responses,
        request_state: params.request_state,
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
