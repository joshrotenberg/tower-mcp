//! Builder for [`McpProxy`].

use std::convert::Infallible;
use std::fmt;
use std::future::Future;

use tokio::sync::mpsc;
use tower::Layer;
use tower::util::BoxCloneService;

use crate::client::{ClientTransport, McpClient};
use crate::error::{Error, Result};
use crate::router::{RouterRequest, RouterResponse};
use crate::transport::CatchError;

use super::backend::{Backend, BackendSetupOptions, ListChanged};
use super::service::{BackendEntry, McpProxy};

/// A backend that was skipped during proxy construction.
#[derive(Debug, thiserror::Error)]
#[error("[{namespace}] {phase:?} failed: {error}")]
pub struct SkippedBackend {
    /// The namespace that was assigned to this backend.
    pub namespace: String,
    /// The error that caused the backend to be skipped.
    #[source]
    pub error: Error,
    /// The phase where the failure occurred.
    pub phase: SkippedPhase,
}

/// Which phase a backend failed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkippedPhase {
    /// Failed to connect the transport.
    Connect,
    /// Failed during initialization or initial capability discovery.
    /// Discovery errors identify the phase in the accompanying error.
    Initialize,
}

/// Source of a proxy build error that includes a catalog discovery failure.
/// Each entry retains its original cause. The standard error chain follows
/// the first reported backend.
#[derive(Debug)]
pub struct BackendSetupFailures {
    /// Backend failures in report order.
    pub skipped: Vec<SkippedBackend>,
}

impl fmt::Display for BackendSetupFailures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, backend) in self.skipped.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            backend.fmt(f)?;
        }
        Ok(())
    }
}

impl std::error::Error for BackendSetupFailures {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.skipped.first().map(|backend| backend as _)
    }
}

/// Result of building an [`McpProxy`], including any backends that were skipped.
pub struct ProxyBuildResult {
    /// The constructed proxy.
    pub proxy: McpProxy,
    /// Backends skipped because a setup phase failed.
    pub skipped: Vec<SkippedBackend>,
}

/// Pending backend before the proxy is built.
struct PendingBackend {
    namespace: String,
    backend: Backend,
    invalidation_rx: Option<mpsc::Receiver<ListChanged>>,
    options: Option<BackendSetupOptions>,
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
    last_connection_failed: bool,
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
            last_connection_failed: false,
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
    pub fn backend_client(self, namespace: impl Into<String>, client: McpClient) -> Self {
        self.push_client(namespace, client, None)
    }

    /// Add an already connected client with bounded initialization and discovery.
    ///
    /// This requires the MCP initialize lifecycle. An existing initialize result
    /// is reused. The connection deadline does not apply and the proxy does not
    /// install a notification handler.
    pub fn backend_client_with_options(
        self,
        namespace: impl Into<String>,
        client: McpClient,
        options: BackendSetupOptions,
    ) -> Self {
        self.push_client(namespace, client, Some(options))
    }

    fn push_client(
        mut self,
        namespace: impl Into<String>,
        client: McpClient,
        options: Option<BackendSetupOptions>,
    ) -> Self {
        let backend = Backend::from_client(namespace, client, self.separator.clone());
        self.pending.push(PendingBackend {
            namespace: backend.namespace.clone(),
            backend,
            invalidation_rx: None,
            options,
            custom_service: None,
        });
        self.last_connection_failed = false;
        self
    }

    /// Add a backend from a [`ClientTransport`].
    ///
    /// The transport will be connected immediately with a notification handler
    /// that watches for list-changed events. Initialization happens during
    /// [`build()`](Self::build).
    pub async fn backend(
        self,
        namespace: impl Into<String>,
        transport: impl ClientTransport,
    ) -> Self {
        self.connect_backend(namespace.into(), async { Ok(transport) }, None)
            .await
    }

    /// Add a backend with distinct setup deadlines and capability-aware discovery.
    ///
    /// The transport future runs immediately. Initialization and discovery
    /// run during [`build`](Self::build). Connection failures are recorded
    /// for the build result.
    pub async fn backend_with_options<T: ClientTransport>(
        self,
        namespace: impl Into<String>,
        transport: impl Future<Output = Result<T>> + Send,
        options: BackendSetupOptions,
    ) -> Self {
        self.connect_backend(namespace.into(), transport, Some(options))
            .await
    }

    async fn connect_backend<T: ClientTransport>(
        mut self,
        namespace: String,
        transport: impl Future<Output = Result<T>> + Send,
        options: Option<BackendSetupOptions>,
    ) -> Self {
        if let Err(error) = self
            .push_transport(namespace.clone(), transport, options)
            .await
        {
            tracing::error!(%namespace, %error, "Failed to connect backend");
            self.connection_failures
                .push(ConnectionFailure { namespace, error });
            self.last_connection_failed = true;
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
        self.push_transport(namespace.into(), async { Ok(transport) }, None)
            .await?;
        Ok(self)
    }

    /// Like [`backend_with_options`](Self::backend_with_options), returning
    /// a transport connection error immediately.
    pub async fn backend_try_with_options<T: ClientTransport>(
        mut self,
        namespace: impl Into<String>,
        transport: impl Future<Output = Result<T>> + Send,
        options: BackendSetupOptions,
    ) -> Result<Self> {
        self.push_transport(namespace.into(), transport, Some(options))
            .await?;
        Ok(self)
    }

    async fn push_transport<T: ClientTransport>(
        &mut self,
        namespace: String,
        transport: impl Future<Output = Result<T>> + Send,
        options: Option<BackendSetupOptions>,
    ) -> Result<()> {
        let (invalidation_tx, invalidation_rx) = mpsc::channel(16);

        let backend = Backend::connect(
            namespace.clone(),
            transport,
            self.separator.clone(),
            invalidation_tx,
            options,
        )
        .await?;

        self.pending.push(PendingBackend {
            namespace,
            backend,
            invalidation_rx: Some(invalidation_rx),
            options,
            custom_service: None,
        });

        self.last_connection_failed = false;
        Ok(())
    }

    /// Apply a Tower layer to the most recent backend addition.
    ///
    /// After a failed connection, this is a no-op.
    ///
    /// The layer wraps the backend's dispatch service, allowing standard
    /// Tower middleware (timeout, rate limit, concurrency limit, etc.) to
    /// be applied per-backend.
    ///
    /// Repeated calls stack: each layer wraps the service composed so far,
    /// so the layer added last sees a request first. In the example below a
    /// request passes the timeout, then the rate limiter, then reaches the
    /// backend, and a `ServiceBuilder` composing the same middleware in one
    /// call remains equivalent.
    ///
    /// Layers that produce errors (e.g., `TimeoutLayer`) are automatically
    /// wrapped with [`CatchError`] to convert errors into JSON-RPC error
    /// responses, maintaining the `Error = Infallible` contract.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use std::time::Duration;
    /// use tower::limit::RateLimitLayer;
    /// use tower::timeout::TimeoutLayer;
    ///
    /// let proxy = McpProxy::builder("proxy", "1.0.0")
    ///     .backend("slow", transport).await
    ///     .backend_layer(RateLimitLayer::new(50, Duration::from_secs(1)))
    ///     .backend_layer(TimeoutLayer::new(Duration::from_secs(30)))
    ///     .build()
    ///     .await?;
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if no backend addition has been attempted.
    pub fn backend_layer<L>(mut self, layer: L) -> Self
    where
        L: Layer<BoxCloneService<RouterRequest, RouterResponse, Infallible>> + Send + 'static,
        L::Service: tower_service::Service<RouterRequest, Response = RouterResponse>
            + Clone
            + Send
            + 'static,
        <L::Service as tower_service::Service<RouterRequest>>::Error: fmt::Display + Send,
        <L::Service as tower_service::Service<RouterRequest>>::Future: Send,
    {
        if self.last_connection_failed {
            return self;
        }
        let pending = self
            .pending
            .last_mut()
            .expect("backend_layer called before adding a backend");

        // Wrap the service composed so far, not the raw backend: starting
        // from the base on every call silently discarded all but the most
        // recent layer (#1173).
        let current = pending
            .custom_service
            .take()
            .unwrap_or_else(|| BoxCloneService::new(pending.backend.service()));
        let layered = layer.layer(current);
        // Wrap with CatchError to convert middleware errors to JSON-RPC errors
        let caught = CatchError::new(layered);
        pending.custom_service = Some(BoxCloneService::new(caught));

        self
    }

    /// Build the proxy, initializing all backends concurrently.
    ///
    /// Each backend runs the MCP initialize handshake and discovers its
    /// capabilities (tools, resources, prompts). Backends that fail setup
    /// are logged and skipped.
    ///
    /// Returns a [`ProxyBuildResult`] containing the proxy and any backends
    /// that were skipped due to setup failures. Check
    /// `result.skipped` to see which backends failed and why.
    ///
    /// For backends added via [`backend()`](Self::backend), a background task
    /// is spawned that watches for list-changed notifications and automatically
    /// refreshes the affected cache.
    ///
    /// # Errors
    ///
    /// Returns an error if no backends were configured or if all backends
    /// failed setup.
    pub async fn build(self) -> Result<ProxyBuildResult> {
        self.build_with_policy(false).await
    }

    async fn build_with_policy(mut self, strict: bool) -> Result<ProxyBuildResult> {
        if self.pending.is_empty() && self.connection_failures.is_empty() {
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
                    match pb.backend.finish_setup(&name, &version, pb.options).await {
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
                            Ok(pb)
                        }
                        Err(e) => {
                            tracing::error!(
                                namespace = %pb.namespace,
                                error = %e,
                                "Backend setup failed, skipping"
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
                invalidation_rxs.push((pb.backend.id, rx));
            }

            entries.push(entry);
            backends.push(pb.backend);
        }

        if backends.is_empty() || (strict && !skipped.is_empty()) {
            let context = if backends.is_empty() {
                "All backends failed setup"
            } else {
                skipped.truncate(1);
                "Backend setup failed"
            };
            let failures = BackendSetupFailures { skipped };
            if failures
                .skipped
                .iter()
                .any(|backend| matches!(backend.error, Error::ProxySetup { .. }))
            {
                return Err(Error::ProxySetup {
                    context: context.to_owned(),
                    source: Box::new(failures),
                });
            }
            // Keep the existing JSON-RPC build error for other setup failures.
            return Err(Error::internal(format!("{context}: {failures}")));
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
        for (backend_id, rx) in invalidation_rxs {
            proxy.spawn_invalidation_watcher(backend_id, rx);
        }

        Ok(ProxyBuildResult { proxy, skipped })
    }

    /// Build the proxy, failing if any backend fails setup.
    ///
    /// Unlike [`build()`](Self::build) which skips failed backends,
    /// this method requires every backend to complete setup.
    ///
    /// # Errors
    ///
    /// Returns a setup error after all backends have finished their attempts.
    /// If every backend fails, the error describes all failures.
    pub async fn build_strict(self) -> Result<McpProxy> {
        Ok(self.build_with_policy(true).await?.proxy)
    }
}
