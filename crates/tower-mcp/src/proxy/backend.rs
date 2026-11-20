//! Backend connection management.
//!
//! Each backend wraps an [`McpClient`] with cached capability discovery,
//! namespace prefixing, and a Tower service for dispatching routed requests.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::{RwLock, mpsc};
use tower_service::Service;

use crate::client::{ClientTransport, McpClient, NotificationHandler};
use crate::error::Error;
use crate::protocol::{
    InitializeResult, McpRequest, McpResponse, PromptDefinition, ResourceDefinition,
    ResourceTemplateDefinition, ToolDefinition,
};
use crate::router::{RouterRequest, RouterResponse};
use tower_mcp_types::JsonRpcError;

type Result<T> = std::result::Result<T, crate::error::Error>;

/// Per-backend deadlines and opt-in capability-aware discovery.
///
/// Defaults are 10 seconds for transport connection, 15 seconds for MCP
/// initialize, and 30 seconds for initial discovery. Each budget starts when
/// its phase starts. Routed RPC middleware and notification refreshes use
/// their own existing behavior.
///
/// Pass transport construction as a future to include it in the connection
/// budget. Work done before that future is polled is outside the budget.
/// For transports that defer network I/O until the first RPC (such as HTTP),
/// that I/O is part of the initialize budget.
///
/// A timeout drops the phase future. A rejected backend drops its client,
/// which aborts the message loop and drops the transport. Use cancellation-safe
/// transport construction and drop-based transport cleanup. Setup does not
/// wait for graceful close on timeout. Legacy HTTP session deletion on drop
/// is best effort. A server-issued session can remain active until the
/// server expires it, including when setup ends before the session ID arrives.
#[derive(Debug, Clone, Copy)]
pub struct BackendSetupOptions {
    /// Time to construct the transport and attach the MCP client.
    pub connect_timeout: Duration,
    /// Time for the MCP initialize exchange, including the initialized notification.
    pub initialize_timeout: Duration,
    /// Time for all initial catalog requests, including all pagination.
    pub discovery_timeout: Duration,
}

impl Default for BackendSetupOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            initialize_timeout: Duration::from_secs(15),
            discovery_timeout: Duration::from_secs(30),
        }
    }
}

async fn setup_phase<T>(
    deadline: Option<Duration>,
    phase: &str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    match deadline {
        Some(duration) => tokio::time::timeout(duration, future)
            .await
            .map_err(|_| Error::Transport(format!("{phase} timed out after {duration:?}")))?,
        None => future.await,
    }
}

/// Fetch one initial catalog. A successful empty catalog and an unadvertised
/// catalog are both empty in the proxy cache. Only the former sends a request.
async fn discover_catalog<T>(
    advertised: bool,
    method: &str,
    fetch: impl Future<Output = Result<Vec<T>>>,
) -> Result<Vec<T>> {
    if !advertised {
        return Ok(Vec::new());
    }
    fetch.await.map_err(|error| Error::ProxySetup {
        context: format!("initial capability discovery failed for {method}"),
        source: Box::new(error),
    })
}

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

/// Source of [`Backend::id`] values; never reused within a process.
static NEXT_BACKEND_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_backend_id() -> u64 {
    NEXT_BACKEND_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// A connected backend MCP server with namespace and cached capabilities.
///
/// Every field is a cheap handle, so a clone shares the connection and the
/// cache with the original.
#[derive(Clone)]
pub(crate) struct Backend {
    /// Process-unique identity of this connection. A replacement under the
    /// same namespace gets a new id, so anything that tracks a backend over
    /// time (the invalidation watcher) keys on this rather than on the
    /// namespace or a position in a list.
    pub id: u64,
    /// Namespace prefix for this backend's capabilities.
    pub namespace: String,
    /// Separator between namespace and name (default: `_`).
    pub separator: String,
    /// The connected MCP client (shared with BackendService).
    pub client: Arc<McpClient>,
    /// Cached capabilities, refreshed on list-changed notifications.
    pub cache: Arc<RwLock<CachedCapabilities>>,
    /// Instructions from the backend's initialize response.
    pub instructions: Option<String>,
}

impl Backend {
    /// Connect a transport and create a backend with a notification handler
    /// that sends invalidation signals on the provided channel.
    pub async fn connect(
        namespace: impl Into<String>,
        transport: impl Future<Output = Result<impl ClientTransport>>,
        separator: String,
        invalidation_tx: mpsc::Sender<ListChanged>,
        options: Option<BackendSetupOptions>,
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

        let client = setup_phase(
            options.map(|options| options.connect_timeout),
            "transport connect",
            async { McpClient::connect_with_handler(transport.await?, handler).await },
        )
        .await?;
        let cache = Arc::new(RwLock::new(CachedCapabilities::default()));

        Ok(Self {
            id: next_backend_id(),
            namespace: namespace.into(),
            separator,
            client: Arc::new(client),
            cache,
            instructions: None,
        })
    }

    /// Create a backend from an already-connected client (no notification forwarding).
    pub fn from_client(namespace: impl Into<String>, client: McpClient, separator: String) -> Self {
        let instructions = client
            .server_info_blocking()
            .and_then(|info| info.instructions.clone());
        Self {
            id: next_backend_id(),
            namespace: namespace.into(),
            separator,
            client: Arc::new(client),
            cache: Arc::new(RwLock::new(CachedCapabilities::default())),
            instructions,
        }
    }

    /// Create a [`BackendService`] for dispatching routed requests to this backend.
    pub fn service(&self) -> BackendService {
        BackendService {
            client: Arc::clone(&self.client),
        }
    }

    /// Run the initialize handshake and discover capabilities before publishing the cache.
    pub async fn finish_setup(
        &mut self,
        proxy_name: &str,
        proxy_version: &str,
        options: Option<BackendSetupOptions>,
    ) -> Result<()> {
        let result = setup_phase(
            options.map(|options| options.initialize_timeout),
            "MCP initialize",
            async {
                if options.is_some() && self.client.is_initialized() {
                    return self.client.server_info().await.ok_or_else(|| {
                        Error::internal("backend client has no MCP initialize result")
                    });
                }
                self.client.initialize(proxy_name, proxy_version).await
            },
        )
        .await?;

        let cache = match options {
            Some(options) => {
                setup_phase(
                    Some(options.discovery_timeout),
                    "initial capability discovery",
                    self.discover_capabilities(&result),
                )
                .await?
            }
            None => self.discover_capabilities_lenient().await,
        };
        *self.cache.write().await = cache;
        self.instructions = result.instructions;
        Ok(())
    }

    async fn discover_capabilities(&self, info: &InitializeResult) -> Result<CachedCapabilities> {
        let capabilities = &info.capabilities;
        let (tools, resources, resource_templates, prompts) = tokio::try_join!(
            discover_catalog(
                capabilities.tools.is_some(),
                "tools/list",
                self.client.list_all_tools()
            ),
            discover_catalog(
                capabilities.resources.is_some(),
                "resources/list",
                self.client.list_all_resources()
            ),
            discover_catalog(
                capabilities.resources.is_some(),
                "resources/templates/list",
                self.client.list_optional_resource_templates()
            ),
            discover_catalog(
                capabilities.prompts.is_some(),
                "prompts/list",
                self.client.list_all_prompts()
            ),
        )?;
        Ok(CachedCapabilities {
            tools,
            resources,
            resource_templates,
            prompts,
        })
    }

    async fn discover_capabilities_lenient(&self) -> CachedCapabilities {
        let (tools, resources, templates, prompts) = tokio::join!(
            self.client.list_all_tools(),
            self.client.list_all_resources(),
            self.client.list_all_resource_templates(),
            self.client.list_all_prompts(),
        );

        CachedCapabilities {
            tools: tools.unwrap_or_default(),
            resources: resources.unwrap_or_default(),
            resource_templates: templates.unwrap_or_default(),
            prompts: prompts.unwrap_or_default(),
        }
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
}

// =============================================================================
// BackendService
// =============================================================================

/// A Tower [`Service`] that dispatches routed requests to a backend [`McpClient`].
///
/// This service handles individual requests after the proxy has already
/// determined which backend they belong to and stripped the namespace prefix.
/// It converts `McpClient` method calls into the `Service<RouterRequest>`
/// interface, allowing standard Tower middleware to wrap per-backend dispatch.
///
/// Created via `Backend::service()` and typically wrapped with middleware
/// before being type-erased into the proxy's backend entry.
#[derive(Clone)]
pub struct BackendService {
    client: Arc<McpClient>,
}

impl Service<RouterRequest> for BackendService {
    type Response = RouterResponse;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<RouterResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let client = Arc::clone(&self.client);
        let request_id = req.id.clone();

        Box::pin(async move {
            let result = dispatch_to_client(&client, req.inner).await;
            Ok(RouterResponse {
                id: request_id,
                inner: result,
            })
        })
    }
}

/// Dispatch a single (namespace-stripped) MCP request to the backend client.
async fn dispatch_to_client(
    client: &McpClient,
    request: McpRequest,
) -> std::result::Result<McpResponse, JsonRpcError> {
    match request {
        McpRequest::CallTool(params) => {
            let result = client
                .call_tool(&params.name, params.arguments)
                .await
                .map_err(|e| JsonRpcError::internal_error(format!("Backend error: {}", e)))?;
            Ok(McpResponse::CallTool(result))
        }
        McpRequest::ReadResource(params) => {
            let result = client
                .read_resource(&params.uri)
                .await
                .map_err(|e| JsonRpcError::internal_error(format!("Backend error: {}", e)))?;
            Ok(McpResponse::ReadResource(result))
        }
        McpRequest::GetPrompt(params) => {
            let args = if params.arguments.is_empty() {
                None
            } else {
                Some(params.arguments)
            };
            let result = client
                .get_prompt(&params.name, args)
                .await
                .map_err(|e| JsonRpcError::internal_error(format!("Backend error: {}", e)))?;
            Ok(McpResponse::GetPrompt(result))
        }
        _ => Err(JsonRpcError::method_not_found(
            "Method not routable to backend",
        )),
    }
}
