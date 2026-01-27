//! MCP Router - routes requests to tools, resources, and prompts
//!
//! The router implements Tower's `Service` trait, making it composable with
//! standard tower middleware.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower_service::Service;

use crate::error::{Error, JsonRpcError, Result};
use crate::protocol::*;
use crate::tool::Tool;

/// MCP Router that dispatches requests to registered handlers
///
/// Implements `tower::Service<McpRequest>` for middleware composition.
///
/// # Example
///
/// ```rust,ignore
/// let router = McpRouter::new()
///     .server_info("my-server", "1.0.0")
///     .tool(evaluate_tool)
///     .tool(validate_tool);
///
/// // Use with tower middleware
/// let service = ServiceBuilder::new()
///     .layer(TracingLayer::new())
///     .service(router);
/// ```
pub struct McpRouter {
    server_name: String,
    server_version: String,
    tools: HashMap<String, Arc<Tool>>,
}

impl McpRouter {
    /// Create a new MCP router
    pub fn new() -> Self {
        Self {
            server_name: "tower-mcp".to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            tools: HashMap::new(),
        }
    }

    /// Set server info
    pub fn server_info(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.server_name = name.into();
        self.server_version = version.into();
        self
    }

    /// Register a tool
    pub fn tool(mut self, tool: Tool) -> Self {
        self.tools.insert(tool.name.clone(), Arc::new(tool));
        self
    }

    /// Get server capabilities based on registered handlers
    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities {
            tools: if self.tools.is_empty() {
                None
            } else {
                Some(ToolsCapability::default())
            },
            resources: None,
            prompts: None,
        }
    }

    /// Handle an MCP request
    async fn handle(&self, request: McpRequest) -> Result<McpResponse> {
        match request {
            McpRequest::Initialize(params) => {
                tracing::info!(
                    client = %params.client_info.name,
                    version = %params.client_info.version,
                    "Client initialized"
                );

                Ok(McpResponse::Initialize(InitializeResult {
                    protocol_version: params.protocol_version,
                    capabilities: self.capabilities(),
                    server_info: Implementation {
                        name: self.server_name.clone(),
                        version: self.server_version.clone(),
                    },
                }))
            }

            McpRequest::ListTools(_params) => {
                let tools: Vec<ToolDefinition> =
                    self.tools.values().map(|t| t.definition()).collect();

                Ok(McpResponse::ListTools(ListToolsResult {
                    tools,
                    next_cursor: None,
                }))
            }

            McpRequest::CallTool(params) => {
                let tool = self
                    .tools
                    .get(&params.name)
                    .ok_or_else(|| Error::JsonRpc(JsonRpcError::method_not_found(&params.name)))?;

                tracing::debug!(tool = %params.name, "Calling tool");
                let result = tool.call(params.arguments).await?;

                Ok(McpResponse::CallTool(result))
            }

            McpRequest::ListResources(_params) => {
                Ok(McpResponse::ListResources(ListResourcesResult {
                    resources: vec![],
                    next_cursor: None,
                }))
            }

            McpRequest::ReadResource(params) => Err(Error::JsonRpc(
                JsonRpcError::method_not_found(&format!("Resource not found: {}", params.uri)),
            )),

            McpRequest::ListPrompts(_params) => Ok(McpResponse::ListPrompts(ListPromptsResult {
                prompts: vec![],
                next_cursor: None,
            })),

            McpRequest::GetPrompt(params) => Err(Error::JsonRpc(JsonRpcError::method_not_found(
                &format!("Prompt not found: {}", params.name),
            ))),

            McpRequest::Ping => Ok(McpResponse::Pong(EmptyResult {})),

            McpRequest::Unknown { method, .. } => {
                Err(Error::JsonRpc(JsonRpcError::method_not_found(&method)))
            }
        }
    }
}

impl Default for McpRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for McpRouter {
    fn clone(&self) -> Self {
        Self {
            server_name: self.server_name.clone(),
            server_version: self.server_version.clone(),
            tools: self.tools.clone(),
        }
    }
}

// =============================================================================
// Tower Service implementation
// =============================================================================

/// Request type for the tower Service implementation
#[derive(Debug)]
pub struct RouterRequest {
    pub id: RequestId,
    pub inner: McpRequest,
}

/// Response type for the tower Service implementation
#[derive(Debug)]
pub struct RouterResponse {
    pub id: RequestId,
    pub inner: std::result::Result<McpResponse, JsonRpcError>,
}

impl RouterResponse {
    /// Convert to JSON-RPC response
    pub fn into_jsonrpc(self) -> JsonRpcResponse {
        match self.inner {
            Ok(response) => {
                let result = serde_json::to_value(response).unwrap_or(serde_json::Value::Null);
                JsonRpcResponse::result(self.id, result)
            }
            Err(error) => JsonRpcResponse::error(Some(self.id), error),
        }
    }
}

impl Service<RouterRequest> for McpRouter {
    type Response = RouterResponse;
    type Error = std::convert::Infallible; // Errors are in the response
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let router = self.clone();
        Box::pin(async move {
            let result = router.handle(req.inner).await;
            Ok(RouterResponse {
                id: req.id,
                inner: result.map_err(|e| match e {
                    Error::JsonRpc(err) => err,
                    Error::Tool(msg) => JsonRpcError::internal_error(msg),
                    e => JsonRpcError::internal_error(e.to_string()),
                }),
            })
        })
    }
}

// =============================================================================
// JSON-RPC layer
// =============================================================================

/// Service that handles JSON-RPC framing
///
/// Wraps an McpRouter and handles JSON-RPC request/response conversion.
pub struct JsonRpcService<S> {
    inner: S,
}

impl<S> JsonRpcService<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S> Clone for JsonRpcService<S>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S> Service<JsonRpcRequest> for JsonRpcService<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    type Response = JsonRpcResponse;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(|_| unreachable!())
    }

    fn call(&mut self, req: JsonRpcRequest) -> Self::Future {
        let mut inner = self.inner.clone();
        Box::pin(async move {
            // Parse the MCP request from JSON-RPC
            let mcp_request = McpRequest::from_jsonrpc(&req)?;

            // Create router request
            let router_req = RouterRequest {
                id: req.id,
                inner: mcp_request,
            };

            // Call the inner service
            let response = inner.call(router_req).await.unwrap(); // Infallible

            // Convert to JSON-RPC response
            Ok(response.into_jsonrpc())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolBuilder;
    use schemars::JsonSchema;
    use serde::Deserialize;
    use tower::ServiceExt;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct AddInput {
        a: i64,
        b: i64,
    }

    #[tokio::test]
    async fn test_router_list_tools() {
        let add_tool = ToolBuilder::new("add")
            .description("Add two numbers")
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build();

        let mut router = McpRouter::new().tool(add_tool);

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams::default()),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ListTools(result)) => {
                assert_eq!(result.tools.len(), 1);
                assert_eq!(result.tools[0].name, "add");
            }
            _ => panic!("Expected ListTools response"),
        }
    }

    #[tokio::test]
    async fn test_router_call_tool() {
        let add_tool = ToolBuilder::new("add")
            .description("Add two numbers")
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build();

        let mut router = McpRouter::new().tool(add_tool);

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "add".to_string(),
                arguments: serde_json::json!({"a": 2, "b": 3}),
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::CallTool(result)) => {
                assert!(!result.is_error);
                // Check the text content
                match &result.content[0] {
                    Content::Text { text } => assert_eq!(text, "5"),
                    _ => panic!("Expected text content"),
                }
            }
            _ => panic!("Expected CallTool response"),
        }
    }

    #[tokio::test]
    async fn test_jsonrpc_service() {
        let add_tool = ToolBuilder::new("add")
            .description("Add two numbers")
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build();

        let router = McpRouter::new().tool(add_tool);
        let mut service = JsonRpcService::new(router);

        let req = JsonRpcRequest::new(1, "tools/list");

        let resp = service.ready().await.unwrap().call(req).await.unwrap();

        match resp {
            JsonRpcResponse::Result(r) => {
                assert_eq!(r.id, RequestId::Number(1));
                let tools = r.result.get("tools").unwrap().as_array().unwrap();
                assert_eq!(tools.len(), 1);
            }
            JsonRpcResponse::Error(_) => panic!("Expected success response"),
        }
    }
}
