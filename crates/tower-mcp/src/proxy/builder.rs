//! Builder for [`McpProxy`].

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::client::{ClientTransport, McpClient};
use crate::error::{Error, Result};

use super::backend::{Backend, ListChanged};
use super::service::McpProxy;

/// Pending backend before the proxy is built.
enum PendingBackend {
    /// Connected with notification handler (from `.backend()` with transport).
    Connected {
        namespace: String,
        backend: Backend,
        invalidation_rx: mpsc::Receiver<ListChanged>,
    },
    /// Pre-connected client (from `.backend_client()`), no notification forwarding.
    Client {
        namespace: String,
        client: McpClient,
    },
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
pub struct McpProxyBuilder {
    name: String,
    version: String,
    separator: String,
    pending: Vec<PendingBackend>,
}

impl McpProxyBuilder {
    /// Create a new proxy builder.
    pub(crate) fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            separator: "_".to_string(),
            pending: Vec::new(),
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

    /// Add a backend from a connected [`McpClient`].
    ///
    /// Note: backends added this way will not have automatic cache refresh
    /// on list-changed notifications. Use [`backend()`](Self::backend) with
    /// a transport for full notification support.
    pub fn backend_client(mut self, namespace: impl Into<String>, client: McpClient) -> Self {
        self.pending.push(PendingBackend::Client {
            namespace: namespace.into(),
            client,
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
                self.pending.push(PendingBackend::Connected {
                    namespace,
                    backend,
                    invalidation_rx,
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
            .map(|pb| match pb {
                PendingBackend::Connected { namespace, .. } => namespace.as_str(),
                PendingBackend::Client { namespace, .. } => namespace.as_str(),
            })
            .collect();
        {
            let mut sorted = namespaces.clone();
            sorted.sort();
            sorted.dedup();
            if sorted.len() != namespaces.len() {
                return Err(Error::internal("Duplicate backend namespaces"));
            }
        }

        // Split pending into backends + invalidation receivers
        let mut init_items = Vec::new();
        for pb in self.pending {
            match pb {
                PendingBackend::Connected {
                    backend,
                    invalidation_rx,
                    ..
                } => {
                    init_items.push((backend, Some(invalidation_rx)));
                }
                PendingBackend::Client { namespace, client } => {
                    let backend = Backend::from_client(namespace, client, self.separator.clone());
                    init_items.push((backend, None));
                }
            }
        }

        // Initialize all backends concurrently
        let name = self.name.clone();
        let version = self.version.clone();
        let init_futures: Vec<_> = init_items
            .into_iter()
            .map(|(backend, invalidation_rx)| {
                let name = name.clone();
                let version = version.clone();
                async move {
                    match backend.initialize(&name, &version).await {
                        Ok(()) => {
                            {
                                let cache = backend.cache.read().await;
                                tracing::info!(
                                    namespace = %backend.namespace,
                                    tools = cache.tools.len(),
                                    resources = cache.resources.len(),
                                    prompts = cache.prompts.len(),
                                    "Backend initialized"
                                );
                            }
                            Some((backend, invalidation_rx))
                        }
                        Err(e) => {
                            tracing::error!(
                                namespace = %backend.namespace,
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
        let mut invalidation_rxs = Vec::new();

        for item in results.into_iter().flatten() {
            let (backend, rx) = item;
            if let Some(rx) = rx {
                invalidation_rxs.push((backends.len(), rx));
            }
            backends.push(backend);
        }

        if backends.is_empty() {
            return Err(Error::internal("All backends failed to initialize"));
        }

        let proxy = McpProxy::new(self.name, self.version, backends);

        // Spawn invalidation watchers for backends with notification handlers
        for (backend_idx, mut rx) in invalidation_rxs {
            let inner = Arc::clone(&proxy.inner);
            tokio::spawn(async move {
                while let Some(changed) = rx.recv().await {
                    let backend = &inner.backends[backend_idx];
                    tracing::debug!(
                        namespace = %backend.namespace,
                        kind = ?changed,
                        "Backend list changed, refreshing cache"
                    );
                    match changed {
                        ListChanged::Tools => backend.refresh_tools().await,
                        ListChanged::Resources => backend.refresh_resources().await,
                        ListChanged::Prompts => backend.refresh_prompts().await,
                    }
                }
            });
        }

        Ok(proxy)
    }
}
