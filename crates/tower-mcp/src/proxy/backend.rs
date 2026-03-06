//! Backend connection management.
//!
//! Each backend wraps an [`McpClient`] with cached capability discovery,
//! namespace prefixing, and a Tower service for dispatching routed requests.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::sync::{RwLock, mpsc};
use tower_service::Service;

use crate::client::{ClientTransport, McpClient, NotificationHandler};
use crate::protocol::{
    McpRequest, McpResponse, PromptDefinition, ResourceDefinition, ResourceTemplateDefinition,
    ToolDefinition,
};
use crate::router::{RouterRequest, RouterResponse};
use tower_mcp_types::JsonRpcError;

type Result<T> = std::result::Result<T, crate::error::Error>;

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
            client: Arc::new(client),
            cache,
            instructions: None,
        })
    }

    /// Create a backend from an already-connected client (no notification forwarding).
    pub fn from_client(namespace: impl Into<String>, client: McpClient, separator: String) -> Self {
        let instructions = client
            .server_info()
            .and_then(|info| info.instructions.clone());
        Self {
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

    /// Initialize the backend: run MCP initialize handshake and discover capabilities.
    ///
    /// Returns the backend's instructions (if any) from the initialize response.
    pub async fn initialize(
        &self,
        proxy_name: &str,
        proxy_version: &str,
    ) -> Result<Option<String>> {
        let result = self.client.initialize(proxy_name, proxy_version).await?;
        let instructions = result.instructions.clone();
        self.refresh_capabilities().await?;
        Ok(instructions)
    }

    /// Refresh cached capabilities from the backend.
    pub async fn refresh_capabilities(&self) -> Result<()> {
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
