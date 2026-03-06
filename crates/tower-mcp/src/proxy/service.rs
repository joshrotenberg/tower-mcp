//! Core proxy service implementing `Service<RouterRequest>`.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower_service::Service;

use crate::protocol::{
    CallToolParams, GetPromptParams, Implementation, InitializeResult, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, McpRequest, McpResponse,
    ReadResourceParams, ServerCapabilities, ToolsCapability,
};
use crate::router::{RouterRequest, RouterResponse};
use tower_mcp_types::JsonRpcError;

use super::backend::Backend;
use super::builder::McpProxyBuilder;

/// An MCP proxy that aggregates multiple backend servers.
///
/// Implements `Service<RouterRequest>` so it can be used with any tower-mcp
/// transport (HTTP, WebSocket, stdio) and composed with tower middleware.
#[derive(Clone)]
pub struct McpProxy {
    pub(super) inner: Arc<McpProxyInner>,
}

pub(super) struct McpProxyInner {
    name: String,
    version: String,
    pub(super) backends: Vec<Backend>,
}

impl McpProxy {
    /// Create a builder for configuring the proxy.
    pub fn builder(name: impl Into<String>, version: impl Into<String>) -> McpProxyBuilder {
        McpProxyBuilder::new(name, version)
    }

    /// Create a new proxy with the given backends (called by builder).
    pub(crate) fn new(name: String, version: String, backends: Vec<Backend>) -> Self {
        Self {
            inner: Arc::new(McpProxyInner {
                name,
                version,
                backends,
            }),
        }
    }

    /// Handle an MCP request by dispatching to the appropriate backend.
    async fn handle_request(
        inner: Arc<McpProxyInner>,
        request: McpRequest,
    ) -> Result<McpResponse, JsonRpcError> {
        match request {
            McpRequest::Initialize(_params) => inner.handle_initialize().await,
            McpRequest::Ping => Ok(McpResponse::Pong(Default::default())),
            McpRequest::ListTools(_params) => inner.handle_list_tools().await,
            McpRequest::CallTool(params) => inner.handle_call_tool(params).await,
            McpRequest::ListResources(_params) => inner.handle_list_resources().await,
            McpRequest::ListResourceTemplates(_params) => {
                inner.handle_list_resource_templates().await
            }
            McpRequest::ReadResource(params) => inner.handle_read_resource(params).await,
            McpRequest::ListPrompts(_params) => inner.handle_list_prompts().await,
            McpRequest::GetPrompt(params) => inner.handle_get_prompt(params).await,
            _ => Err(JsonRpcError::method_not_found(
                "Method not supported by proxy",
            )),
        }
    }
}

impl McpProxyInner {
    /// Find the backend that owns a namespaced tool name.
    /// Returns the backend and the stripped (original) name.
    fn route_tool(&self, name: &str) -> Option<(&Backend, String)> {
        for backend in &self.backends {
            if let Some(stripped) = backend.strip_prefix(name) {
                return Some((backend, stripped.to_string()));
            }
        }
        None
    }

    /// Find the backend that owns a namespaced resource URI.
    fn route_resource(&self, uri: &str) -> Option<(&Backend, String)> {
        for backend in &self.backends {
            if let Some(stripped) = backend.strip_uri_prefix(uri) {
                return Some((backend, stripped.to_string()));
            }
        }
        None
    }

    /// Find the backend that owns a namespaced prompt name.
    fn route_prompt(&self, name: &str) -> Option<(&Backend, String)> {
        for backend in &self.backends {
            if let Some(stripped) = backend.strip_prefix(name) {
                return Some((backend, stripped.to_string()));
            }
        }
        None
    }

    async fn handle_initialize(&self) -> Result<McpResponse, JsonRpcError> {
        Ok(McpResponse::Initialize(InitializeResult {
            protocol_version: "2025-11-25".to_string(),
            server_info: Implementation {
                name: self.name.clone(),
                version: self.version.clone(),
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
                self.backends.len()
            )),
            meta: None,
        }))
    }

    async fn handle_list_tools(&self) -> Result<McpResponse, JsonRpcError> {
        // Fan out to all backends concurrently
        let futures: Vec<_> = self.backends.iter().map(|b| b.namespaced_tools()).collect();
        let results = futures::future::join_all(futures).await;

        let tools: Vec<_> = results.into_iter().flatten().collect();
        Ok(McpResponse::ListTools(ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        }))
    }

    async fn handle_call_tool(&self, params: CallToolParams) -> Result<McpResponse, JsonRpcError> {
        let (backend, stripped_name) = self.route_tool(&params.name).ok_or_else(|| {
            JsonRpcError::invalid_params(format!("Unknown tool: {}", params.name))
        })?;

        let result = backend
            .call_tool(&stripped_name, params.arguments)
            .await
            .map_err(|e| JsonRpcError::internal_error(format!("Backend error: {}", e)))?;

        Ok(McpResponse::CallTool(result))
    }

    async fn handle_list_resources(&self) -> Result<McpResponse, JsonRpcError> {
        let futures: Vec<_> = self
            .backends
            .iter()
            .map(|b| b.namespaced_resources())
            .collect();
        let results = futures::future::join_all(futures).await;

        let resources: Vec<_> = results.into_iter().flatten().collect();
        Ok(McpResponse::ListResources(ListResourcesResult {
            resources,
            next_cursor: None,
            meta: None,
        }))
    }

    async fn handle_list_resource_templates(&self) -> Result<McpResponse, JsonRpcError> {
        let futures: Vec<_> = self
            .backends
            .iter()
            .map(|b| b.namespaced_resource_templates())
            .collect();
        let results = futures::future::join_all(futures).await;

        let resource_templates: Vec<_> = results.into_iter().flatten().collect();
        Ok(McpResponse::ListResourceTemplates(
            ListResourceTemplatesResult {
                resource_templates,
                next_cursor: None,
                meta: None,
            },
        ))
    }

    async fn handle_read_resource(
        &self,
        params: ReadResourceParams,
    ) -> Result<McpResponse, JsonRpcError> {
        let (backend, stripped_uri) = self.route_resource(&params.uri).ok_or_else(|| {
            JsonRpcError::invalid_params(format!("Unknown resource: {}", params.uri))
        })?;

        let result = backend
            .read_resource(&stripped_uri)
            .await
            .map_err(|e| JsonRpcError::internal_error(format!("Backend error: {}", e)))?;

        Ok(McpResponse::ReadResource(result))
    }

    async fn handle_list_prompts(&self) -> Result<McpResponse, JsonRpcError> {
        let futures: Vec<_> = self
            .backends
            .iter()
            .map(|b| b.namespaced_prompts())
            .collect();
        let results = futures::future::join_all(futures).await;

        let prompts: Vec<_> = results.into_iter().flatten().collect();
        Ok(McpResponse::ListPrompts(ListPromptsResult {
            prompts,
            next_cursor: None,
            meta: None,
        }))
    }

    async fn handle_get_prompt(
        &self,
        params: GetPromptParams,
    ) -> Result<McpResponse, JsonRpcError> {
        let (backend, stripped_name) = self.route_prompt(&params.name).ok_or_else(|| {
            JsonRpcError::invalid_params(format!("Unknown prompt: {}", params.name))
        })?;

        let result = backend
            .get_prompt(&stripped_name, params.arguments)
            .await
            .map_err(|e| JsonRpcError::internal_error(format!("Backend error: {}", e)))?;

        Ok(McpResponse::GetPrompt(result))
    }
}

impl Service<RouterRequest> for McpProxy {
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let inner = Arc::clone(&self.inner);
        let request_id = req.id.clone();

        Box::pin(async move {
            let result = McpProxy::handle_request(inner, req.inner).await;
            Ok(RouterResponse {
                id: request_id,
                inner: result,
            })
        })
    }
}
