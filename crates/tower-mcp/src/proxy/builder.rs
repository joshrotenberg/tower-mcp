//! Builder for [`McpProxy`].

use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;

use tokio::sync::mpsc;
use tower::Layer;
use tower::util::BoxCloneService;

use crate::client::{ClientTransport, McpClient};
use crate::error::{Error, Result};
use crate::router::{RouterRequest, RouterResponse};
use crate::transport::CatchError;

use super::backend::{Backend, BackendService, ListChanged};
use super::service::{BackendEntry, McpProxy};

/// Pending backend before the proxy is built.
struct PendingBackend {
    namespace: String,
    backend: Backend,
    invalidation_rx: Option<mpsc::Receiver<ListChanged>>,
    /// Type-erased service (set by `.backend_layer()`).
    /// If None, BackendService is used directly.
    custom_service: Option<BoxCloneService<RouterRequest, RouterResponse, Infallible>>,
}

/// Builder for constructing an [`McpProxy`].
///
/// # Example
///
/// ```rust,no_run
/// use tower_mcp::proxy::McpProxy;
/// use tower_mcp::client::StdioClientTransport;
///
/// # async fn example() -> Result<(), tower_mcp::BoxError> {
/// let proxy = McpProxy::builder("my-proxy", "1.0.0")
///     .backend("db", StdioClientTransport::spawn("db-server", &[]).await?)
///     .await
///     .separator(".")
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// # Per-Backend Middleware
///
/// Apply Tower middleware to individual backends using
/// [`backend_layer()`](Self::backend_layer):
///
/// ```rust,ignore
/// use std::time::Duration;
/// use tower::timeout::TimeoutLayer;
///
/// let proxy = McpProxy::builder("proxy", "1.0.0")
///     .backend("slow-api", slow_transport).await
///     .backend_layer(TimeoutLayer::new(Duration::from_secs(60)))
///     .backend("fast-db", fast_transport).await
///     .backend_layer(TimeoutLayer::new(Duration::from_secs(5)))
///     .build()
///     .await?;
/// ```
pub struct McpProxyBuilder {
    name: String,
    version: String,
    separator: String,
    pending: Vec<PendingBackend>,
    notification_tx: Option<crate::context::NotificationSender>,
}

impl McpProxyBuilder {
    /// Create a new proxy builder.
    pub(crate) fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            separator: "_".to_string(),
            pending: Vec::new(),
            notification_tx: None,
        }
    }

    /// Set the namespace separator (default: `_`).
    ///
    /// The separator is inserted between the backend namespace and the
    /// tool/resource/prompt name. For example, with separator `"_"` and
    /// namespace `"db"`, a tool named `"query"` becomes `"db_query"`.
    pub fn separator(mut self, sep: impl Into<String>) -> Self {
        self.separator = sep.into();
        self
    }

    /// Set a notification sender for forwarding backend list-changed
    /// notifications to downstream clients.
    ///
    /// When a backend emits `tools/list_changed`, `resources/list_changed`,
    /// or `prompts/list_changed`, the proxy refreshes its cache and then
    /// forwards the notification through this sender so transports can
    /// relay it to connected clients.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use tower_mcp::context::notification_channel;
    ///
    /// let (notif_tx, notif_rx) = notification_channel(32);
    /// let proxy = McpProxy::builder("proxy", "1.0.0")
    ///     .notification_sender(notif_tx)
    ///     .backend("db", transport).await
    ///     .build().await?;
    ///
    /// let mut transport = GenericStdioTransport::with_notifications(proxy, notif_rx);
    /// ```
    pub fn notification_sender(mut self, tx: crate::context::NotificationSender) -> Self {
        self.notification_tx = Some(tx);
        self
    }

    /// Add a backend from a connected [`McpClient`].
    ///
    /// Note: backends added this way will not have automatic cache refresh
    /// on list-changed notifications. Use [`backend()`](Self::backend) with
    /// a transport for full notification support.
    pub fn backend_client(mut self, namespace: impl Into<String>, client: McpClient) -> Self {
        let backend = Backend::from_client(namespace, client, self.separator.clone());
        self.pending.push(PendingBackend {
            namespace: backend.namespace.clone(),
            backend,
            invalidation_rx: None,
            custom_service: None,
        });
        self
    }

    /// Add a backend from a [`ClientTransport`].
    ///
    /// The transport will be connected immediately with a notification handler
    /// that watches for list-changed events. Initialization happens during
    /// [`build()`](Self::build).
    pub async fn backend(
        mut self,
        namespace: impl Into<String>,
        transport: impl ClientTransport,
    ) -> Self {
        let namespace = namespace.into();
        let (invalidation_tx, invalidation_rx) = mpsc::channel(16);

        match Backend::connect(
            namespace.clone(),
            transport,
            self.separator.clone(),
            invalidation_tx,
        )
        .await
        {
            Ok(backend) => {
                self.pending.push(PendingBackend {
                    namespace,
                    backend,
                    invalidation_rx: Some(invalidation_rx),
                    custom_service: None,
                });
            }
            Err(e) => {
                tracing::error!(
                    namespace = %namespace,
                    error = %e,
                    "Failed to connect backend"
                );
            }
        }
        self
    }

    /// Apply a Tower layer to the most recently added backend.
    ///
    /// The layer wraps the backend's dispatch service, allowing standard
    /// Tower middleware (timeout, rate limit, concurrency limit, etc.) to
    /// be applied per-backend.
    ///
    /// Layers that produce errors (e.g., `TimeoutLayer`) are automatically
    /// wrapped with [`CatchError`] to convert errors into JSON-RPC error
    /// responses, maintaining the `Error = Infallible` contract.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use std::time::Duration;
    /// use tower::timeout::TimeoutLayer;
    ///
    /// let proxy = McpProxy::builder("proxy", "1.0.0")
    ///     .backend("slow", transport).await
    ///     .backend_layer(TimeoutLayer::new(Duration::from_secs(30)))
    ///     .build()
    ///     .await?;
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if no backend has been added yet.
    pub fn backend_layer<L>(mut self, layer: L) -> Self
    where
        L: Layer<BackendService> + Send + 'static,
        L::Service: tower_service::Service<RouterRequest, Response = RouterResponse>
            + Clone
            + Send
            + 'static,
        <L::Service as tower_service::Service<RouterRequest>>::Error: fmt::Display + Send,
        <L::Service as tower_service::Service<RouterRequest>>::Future: Send,
    {
        let pending = self
            .pending
            .last_mut()
            .expect("backend_layer called before adding a backend");

        // Get the base BackendService and apply the layer
        let base = pending.backend.service();
        let layered = layer.layer(base);
        // Wrap with CatchError to convert middleware errors to JSON-RPC errors
        let caught = CatchError::new(layered);
        pending.custom_service = Some(BoxCloneService::new(caught));

        self
    }

    /// Build the proxy, initializing all backends concurrently.
    ///
    /// Each backend runs the MCP initialize handshake and discovers its
    /// capabilities (tools, resources, prompts). Backends that fail to
    /// initialize are logged and skipped.
    ///
    /// For backends added via [`backend()`](Self::backend), a background task
    /// is spawned that watches for list-changed notifications and automatically
    /// refreshes the affected cache.
    pub async fn build(self) -> Result<McpProxy> {
        if self.pending.is_empty() {
            return Err(Error::internal("No backends configured"));
        }

        // Check for duplicate namespaces
        let namespaces: Vec<&str> = self
            .pending
            .iter()
            .map(|pb| pb.namespace.as_str())
            .collect();
        {
            let mut sorted = namespaces.clone();
            sorted.sort();
            sorted.dedup();
            if sorted.len() != namespaces.len() {
                return Err(Error::internal("Duplicate backend namespaces"));
            }
        }

        // Initialize all backends concurrently
        let name = self.name.clone();
        let version = self.version.clone();
        let init_futures: Vec<_> = self
            .pending
            .into_iter()
            .map(|pb| {
                let name = name.clone();
                let version = version.clone();
                async move {
                    match pb.backend.initialize(&name, &version).await {
                        Ok(()) => {
                            {
                                let cache = pb.backend.cache.read().await;
                                tracing::info!(
                                    namespace = %pb.namespace,
                                    tools = cache.tools.len(),
                                    resources = cache.resources.len(),
                                    prompts = cache.prompts.len(),
                                    "Backend initialized"
                                );
                            }
                            Some(pb)
                        }
                        Err(e) => {
                            tracing::error!(
                                namespace = %pb.namespace,
                                error = %e,
                                "Failed to initialize backend, skipping"
                            );
                            None
                        }
                    }
                }
            })
            .collect();

        let results = futures::future::join_all(init_futures).await;

        let mut backends = Vec::new();
        let mut entries = Vec::new();
        let mut invalidation_rxs = Vec::new();

        for pb in results.into_iter().flatten() {
            let entry = if let Some(svc) = pb.custom_service {
                BackendEntry::from_backend_with_service(&pb.backend, svc)
            } else {
                BackendEntry::from_backend(&pb.backend)
            };

            if let Some(rx) = pb.invalidation_rx {
                invalidation_rxs.push((backends.len(), rx));
            }

            entries.push(entry);
            backends.push(pb.backend);
        }

        if backends.is_empty() {
            return Err(Error::internal("All backends failed to initialize"));
        }

        let proxy = McpProxy::new(
            self.name,
            self.version,
            backends,
            entries,
            self.notification_tx,
        );

        // Spawn invalidation watchers for backends with notification handlers.
        // After refreshing the cache, forward list-changed notifications
        // downstream so transports can relay them to connected clients.
        for (backend_idx, mut rx) in invalidation_rxs {
            let shared = Arc::clone(&proxy.shared);
            tokio::spawn(async move {
                while let Some(changed) = rx.recv().await {
                    let backend = &shared.backends[backend_idx];
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

        Ok(proxy)
    }
}
