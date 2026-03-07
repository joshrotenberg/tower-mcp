//! Builder for [`McpProxy`].

use std::convert::Infallible;
use std::fmt;

use tokio::sync::mpsc;
use tower::Layer;
use tower::util::BoxCloneService;

use crate::client::{ClientTransport, McpClient};
use crate::error::{Error, Result};
use crate::router::{RouterRequest, RouterResponse};
use crate::transport::CatchError;

use super::backend::{Backend, BackendService, ListChanged};
use super::service::{BackendEntry, McpProxy};

/// A backend that was skipped during proxy construction.
#[derive(Debug)]
pub struct SkippedBackend {
    /// The namespace that was assigned to this backend.
    pub namespace: String,
    /// The error that caused the backend to be skipped.
    pub error: Error,
    /// The phase where the failure occurred.
    pub phase: SkippedPhase,
}

/// Which phase a backend failed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkippedPhase {
    /// Failed to connect the transport.
    Connect,
    /// Failed during MCP initialization handshake.
    Initialize,
}

impl fmt::Display for SkippedBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}] {:?} failed: {}",
            self.namespace, self.phase, self.error
        )
    }
}

/// Result of building an [`McpProxy`], including any backends that were skipped.
pub struct ProxyBuildResult {
    /// The constructed proxy.
    pub proxy: McpProxy,
    /// Backends that failed to connect or initialize and were skipped.
    pub skipped: Vec<SkippedBackend>,
}

/// Pending backend before the proxy is built.
struct PendingBackend {
    namespace: String,
    backend: Backend,
    invalidation_rx: Option<mpsc::Receiver<ListChanged>>,
    /// Type-erased service (set by `.backend_layer()`).
    /// If None, BackendService is used directly.
    custom_service: Option<BoxCloneService<RouterRequest, RouterResponse, Infallible>>,
}

/// Backends that failed during `backend()` connection, tracked until `build()`.
struct ConnectionFailure {
    namespace: String,
    error: Error,
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
    connection_failures: Vec<ConnectionFailure>,
    /// Custom instructions override. If set, used instead of aggregated backend instructions.
    instructions: Option<String>,
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
            connection_failures: Vec::new(),
            instructions: None,
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

    /// Set custom instructions for the proxy's initialize response.
    ///
    /// When set, this overrides the default behavior of aggregating
    /// backend instructions. Use this to provide a curated description
    /// of the proxy's capabilities.
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
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
                self.connection_failures.push(ConnectionFailure {
                    namespace,
                    error: e,
                });
            }
        }
        self
    }

    /// Add a backend from a transport, returning an error on connection failure.
    ///
    /// Unlike [`backend()`](Self::backend) which silently skips failed connections,
    /// this method returns an error so the caller can decide how to handle it.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let builder = McpProxy::builder("proxy", "1.0.0")
    ///     .backend_try("db", transport).await?;
    /// ```
    pub async fn backend_try(
        mut self,
        namespace: impl Into<String>,
        transport: impl ClientTransport,
    ) -> Result<Self> {
        let namespace = namespace.into();
        let (invalidation_tx, invalidation_rx) = mpsc::channel(16);

        let backend = Backend::connect(
            namespace.clone(),
            transport,
            self.separator.clone(),
            invalidation_tx,
        )
        .await?;

        self.pending.push(PendingBackend {
            namespace,
            backend,
            invalidation_rx: Some(invalidation_rx),
            custom_service: None,
        });

        Ok(self)
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
    /// Returns a [`ProxyBuildResult`] containing the proxy and any backends
    /// that were skipped due to initialization failures. Check
    /// `result.skipped` to see which backends failed and why.
    ///
    /// For backends added via [`backend()`](Self::backend), a background task
    /// is spawned that watches for list-changed notifications and automatically
    /// refreshes the affected cache.
    ///
    /// # Errors
    ///
    /// Returns an error if no backends were configured or if all backends
    /// failed to initialize.
    pub async fn build(mut self) -> Result<ProxyBuildResult> {
        if self.pending.is_empty() {
            return Err(Error::internal("No backends configured"));
        }

        // Ensure all backends use the builder's final separator.
        // This handles the case where `.separator()` is called after `.backend()`.
        for pb in &mut self.pending {
            pb.backend.separator = self.separator.clone();
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

        // Check for ambiguous namespace prefixes.
        // With separator "_", namespaces "redis" and "redis_ft" both produce
        // the prefix "redis_", making "redis_ft_search" ambiguous.
        let prefixes: Vec<String> = namespaces
            .iter()
            .map(|ns| format!("{}{}", ns, self.separator))
            .collect();
        for (i, prefix_i) in prefixes.iter().enumerate() {
            for (j, prefix_j) in prefixes.iter().enumerate() {
                if i != j && prefix_j.starts_with(prefix_i.as_str()) {
                    return Err(Error::internal(format!(
                        "Ambiguous namespace prefixes: \"{}\" and \"{}\" with separator \"{}\". \
                         The prefix \"{}\" is a prefix of \"{}\", which makes routing ambiguous. \
                         Use a different separator (e.g., \".\") or rename the namespaces.",
                        namespaces[i], namespaces[j], self.separator, prefix_i, prefix_j,
                    )));
                }
            }
        }

        // Initialize all backends concurrently
        let name = self.name.clone();
        let version = self.version.clone();
        let init_futures: Vec<_> = self
            .pending
            .into_iter()
            .map(|mut pb| {
                let name = name.clone();
                let version = version.clone();
                async move {
                    match pb.backend.initialize(&name, &version).await {
                        Ok(instructions) => {
                            pb.backend.instructions = instructions;
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
                            Ok(pb)
                        }
                        Err(e) => {
                            tracing::error!(
                                namespace = %pb.namespace,
                                error = %e,
                                "Failed to initialize backend, skipping"
                            );
                            Err(SkippedBackend {
                                namespace: pb.namespace,
                                error: e,
                                phase: SkippedPhase::Initialize,
                            })
                        }
                    }
                }
            })
            .collect();

        let results = futures::future::join_all(init_futures).await;

        let mut backends = Vec::new();
        let mut entries = Vec::new();
        let mut invalidation_rxs = Vec::new();

        // Start with connection failures from the `backend()` phase
        let mut skipped: Vec<SkippedBackend> = self
            .connection_failures
            .into_iter()
            .map(|f| SkippedBackend {
                namespace: f.namespace,
                error: f.error,
                phase: SkippedPhase::Connect,
            })
            .collect();

        for result in results {
            let pb = match result {
                Ok(pb) => pb,
                Err(s) => {
                    skipped.push(s);
                    continue;
                }
            };
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

        // Build instructions: use custom override or aggregate from backends
        let instructions = if let Some(custom) = self.instructions {
            Some(custom)
        } else {
            let mut parts = vec![format!(
                "MCP proxy aggregating {} backend servers.",
                backends.len()
            )];
            for b in &backends {
                if let Some(inst) = &b.instructions {
                    parts.push(format!("[{}] {}", b.namespace, inst));
                }
            }
            // Only include backend details if at least one backend has instructions
            if parts.len() > 1 {
                Some(parts.join("\n\n"))
            } else {
                Some(parts.remove(0))
            }
        };

        let proxy = McpProxy::new(
            self.name,
            self.version,
            backends,
            entries,
            self.notification_tx,
            instructions,
            self.separator.clone(),
        );

        // Spawn invalidation watchers for backends with notification handlers.
        for (backend_idx, rx) in invalidation_rxs {
            proxy.spawn_invalidation_watcher(backend_idx, rx);
        }

        Ok(ProxyBuildResult { proxy, skipped })
    }

    /// Build the proxy, failing if any backend fails to initialize.
    ///
    /// Unlike [`build()`](Self::build) which skips failed backends,
    /// this method returns an error if any backend fails to connect or
    /// initialize.
    ///
    /// # Errors
    ///
    /// Returns the first initialization failure encountered (after
    /// waiting for all backends to attempt initialization).
    pub async fn build_strict(self) -> Result<McpProxy> {
        let result = self.build().await?;
        if let Some(first) = result.skipped.into_iter().next() {
            return Err(Error::internal(format!(
                "Backend \"{}\" failed to initialize: {}",
                first.namespace, first.error
            )));
        }
        Ok(result.proxy)
    }
}
