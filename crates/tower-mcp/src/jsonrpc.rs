//! JSON-RPC 2.0 service layer
//!
//! Provides a Tower [`Layer`] and [`Service`] for JSON-RPC framing of MCP requests.
//!
//! - [`JsonRpcLayer`] - Tower layer for [`ServiceBuilder`](tower::ServiceBuilder) composition
//! - [`JsonRpcService`] - Tower service wrapping an MCP router
//!
//! The service handles:
//! - Single request processing
//! - Batch request processing (concurrent execution)
//! - JSON-RPC version validation
//! - Error conversion to JSON-RPC error responses

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tower::{Layer, ServiceExt};
use tower_service::Service;

use crate::error::{Error, JsonRpcError, Result};
use crate::protocol::{
    JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, JsonRpcResponseMessage, McpRequest,
};
use crate::router::{Extensions, RouterRequest, RouterResponse};

/// Tower layer that adds JSON-RPC 2.0 framing to an MCP service.
///
/// This is the standard way to compose `JsonRpcService` with other tower
/// middleware via [`ServiceBuilder`](tower::ServiceBuilder).
///
/// # Example
///
/// ```rust
/// use tower::ServiceBuilder;
/// use tower_mcp::{McpRouter, JsonRpcLayer, JsonRpcService};
///
/// let router = McpRouter::new().server_info("my-server", "1.0.0");
///
/// // Compose with ServiceBuilder
/// let service = ServiceBuilder::new()
///     .layer(JsonRpcLayer::new())
///     .service(router);
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonRpcLayer {
    _priv: (),
}

impl JsonRpcLayer {
    /// Create a new `JsonRpcLayer`.
    pub fn new() -> Self {
        Self { _priv: () }
    }
}

impl<S> Layer<S> for JsonRpcLayer {
    type Service = JsonRpcService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        JsonRpcService::new(inner)
    }
}

/// Service that handles JSON-RPC framing.
///
/// Wraps an MCP service and handles JSON-RPC request/response conversion.
/// Supports both single requests and batch requests.
///
/// Can be created directly via [`JsonRpcService::new`] or through the
/// [`JsonRpcLayer`] for [`ServiceBuilder`](tower::ServiceBuilder) composition.
///
/// # Example
///
/// ```rust
/// use tower_mcp::{McpRouter, JsonRpcService};
///
/// let router = McpRouter::new().server_info("my-server", "1.0.0");
/// let service = JsonRpcService::new(router);
/// ```
pub struct JsonRpcService<S> {
    inner: S,
    extensions: Extensions,
}

impl<S> JsonRpcService<S> {
    /// Create a new JSON-RPC service wrapping the given inner service
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            extensions: Extensions::new(),
        }
    }

    /// Set extensions to inject into every `RouterRequest` created by this service.
    ///
    /// This is used by transports to bridge data (e.g., `TokenClaims`) from the
    /// HTTP/WebSocket layer into the MCP request pipeline.
    pub fn with_extensions(mut self, ext: Extensions) -> Self {
        self.extensions = ext;
        self
    }

    /// Process a single JSON-RPC request
    pub async fn call_single(&mut self, req: JsonRpcRequest) -> Result<JsonRpcResponse>
    where
        S: Service<RouterRequest, Response = RouterResponse, Error = std::convert::Infallible>
            + Clone
            + Send
            + 'static,
        S::Future: Send,
    {
        process_single_request(self.inner.clone(), req, self.extensions.clone()).await
    }

    /// Process a batch of JSON-RPC requests concurrently
    pub async fn call_batch(
        &mut self,
        requests: Vec<JsonRpcRequest>,
    ) -> Result<Vec<JsonRpcResponse>>
    where
        S: Service<RouterRequest, Response = RouterResponse, Error = std::convert::Infallible>
            + Clone
            + Send
            + 'static,
        S::Future: Send,
    {
        if requests.is_empty() {
            return Err(Error::JsonRpc(JsonRpcError::invalid_request(
                "Empty batch request",
            )));
        }

        // Process all requests concurrently
        let futures: Vec<_> = requests
            .into_iter()
            .map(|req| {
                let inner = self.inner.clone();
                let extensions = self.extensions.clone();
                let req_id = req.id.clone();
                async move {
                    match process_single_request(inner, req, extensions).await {
                        Ok(resp) => resp,
                        Err(e) => {
                            // Convert errors to error responses instead of dropping
                            JsonRpcResponse::error(
                                Some(req_id),
                                JsonRpcError::internal_error(e.to_string()),
                            )
                        }
                    }
                }
            })
            .collect();

        let results: Vec<JsonRpcResponse> = futures::future::join_all(futures).await;

        // Results will never be empty since we converted all errors to responses
        Ok(results)
    }

    /// Process a JSON-RPC message (single or batch)
    pub async fn call_message(&mut self, msg: JsonRpcMessage) -> Result<JsonRpcResponseMessage>
    where
        S: Service<RouterRequest, Response = RouterResponse, Error = std::convert::Infallible>
            + Clone
            + Send
            + 'static,
        S::Future: Send,
    {
        match msg {
            JsonRpcMessage::Single(req) => {
                let response = self.call_single(req).await?;
                Ok(JsonRpcResponseMessage::Single(response))
            }
            JsonRpcMessage::Batch(requests) => {
                let responses = self.call_batch(requests).await?;
                Ok(JsonRpcResponseMessage::Batch(responses))
            }
            _ => Ok(JsonRpcResponseMessage::Single(JsonRpcResponse::error(
                None,
                JsonRpcError::invalid_request("Unsupported message type"),
            ))),
        }
    }
}

impl<S> Clone for JsonRpcService<S>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            extensions: self.extensions.clone(),
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
        let inner = self.inner.clone();
        let extensions = self.extensions.clone();
        Box::pin(async move {
            // Parse the MCP request from JSON-RPC
            let mcp_request = McpRequest::from_jsonrpc(&req)?;

            // Create router request
            let router_req = RouterRequest {
                id: req.id,
                inner: mcp_request,
                extensions,
            };

            // Call the inner service (oneshot handles poll_ready)
            let response = inner.oneshot(router_req).await.unwrap(); // Infallible

            // Convert to JSON-RPC response
            Ok(response.into_jsonrpc())
        })
    }
}

/// Service implementation for JSON-RPC batch requests
impl<S> Service<JsonRpcMessage> for JsonRpcService<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    type Response = JsonRpcResponseMessage;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(|_| unreachable!())
    }

    fn call(&mut self, msg: JsonRpcMessage) -> Self::Future {
        let inner = self.inner.clone();
        let extensions = self.extensions.clone();
        Box::pin(async move {
            match msg {
                JsonRpcMessage::Single(req) => {
                    let response = process_single_request(inner, req, extensions).await?;
                    Ok(JsonRpcResponseMessage::Single(response))
                }
                JsonRpcMessage::Batch(requests) => {
                    if requests.is_empty() {
                        // Empty batch is an invalid request per JSON-RPC spec
                        return Ok(JsonRpcResponseMessage::Single(JsonRpcResponse::error(
                            None,
                            JsonRpcError::invalid_request("Empty batch request"),
                        )));
                    }

                    // Process all requests concurrently
                    let futures: Vec<_> = requests
                        .into_iter()
                        .map(|req| {
                            let inner = inner.clone();
                            let extensions = extensions.clone();
                            let req_id = req.id.clone();
                            async move {
                                match process_single_request(inner, req, extensions).await {
                                    Ok(resp) => resp,
                                    Err(e) => {
                                        // Convert errors to error responses instead of dropping
                                        JsonRpcResponse::error(
                                            Some(req_id),
                                            JsonRpcError::internal_error(e.to_string()),
                                        )
                                    }
                                }
                            }
                        })
                        .collect();

                    let results: Vec<JsonRpcResponse> = futures::future::join_all(futures).await;

                    // Empty results only possible if input was empty (already handled above)
                    if results.is_empty() {
                        return Ok(JsonRpcResponseMessage::Single(JsonRpcResponse::error(
                            None,
                            JsonRpcError::internal_error("All batch requests failed"),
                        )));
                    }

                    Ok(JsonRpcResponseMessage::Batch(results))
                }
                _ => Ok(JsonRpcResponseMessage::Single(JsonRpcResponse::error(
                    None,
                    JsonRpcError::invalid_request("Unsupported message type"),
                ))),
            }
        })
    }
}

/// Helper function to process a single JSON-RPC request
async fn process_single_request<S>(
    inner: S,
    req: JsonRpcRequest,
    extensions: Extensions,
) -> std::result::Result<JsonRpcResponse, Error>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = std::convert::Infallible>
        + Send
        + 'static,
    S::Future: Send,
{
    // Validate JSON-RPC version
    if let Err(e) = req.validate() {
        return Ok(JsonRpcResponse::error(Some(req.id), e));
    }

    // Parse the MCP request from JSON-RPC
    let mcp_request = match McpRequest::from_jsonrpc(&req) {
        Ok(r) => r,
        Err(e) => {
            return Ok(JsonRpcResponse::error(
                Some(req.id),
                JsonRpcError::invalid_params(e.to_string()),
            ));
        }
    };

    // Create router request
    let router_req = RouterRequest {
        id: req.id,
        inner: mcp_request,
        extensions,
    };

    // Call the inner service (oneshot handles poll_ready)
    let response = inner.oneshot(router_req).await.unwrap(); // Infallible

    // Convert to JSON-RPC response
    Ok(response.into_jsonrpc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpRouter;
    use crate::tool::ToolBuilder;
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct AddInput {
        a: i32,
        b: i32,
    }

    fn create_test_router() -> McpRouter {
        let add_tool = ToolBuilder::new("add")
            .description("Add two numbers")
            .handler(|input: AddInput| async move {
                Ok(crate::CallToolResult::text(format!(
                    "{}",
                    input.a + input.b
                )))
            })
            .build();

        McpRouter::new()
            .server_info("test-server", "1.0.0")
            .tool(add_tool)
    }

    #[tokio::test]
    async fn test_jsonrpc_service() {
        let router = create_test_router();
        let mut service = JsonRpcService::new(router.clone());

        // Initialize first
        let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1.0" }
        }));
        let resp = service.call_single(init_req).await.unwrap();
        assert!(matches!(resp, JsonRpcResponse::Result(_)));

        // Mark as initialized
        router.handle_notification(crate::protocol::McpNotification::Initialized);

        // Now list tools
        let req = JsonRpcRequest::new(2, "tools/list").with_params(serde_json::json!({}));
        let resp = service.call_single(req).await.unwrap();

        match resp {
            JsonRpcResponse::Result(r) => {
                let tools = r.result.get("tools").unwrap().as_array().unwrap();
                assert_eq!(tools.len(), 1);
            }
            JsonRpcResponse::Error(e) => panic!("Expected result, got error: {:?}", e),
            _ => panic!("unexpected response variant"),
        }
    }

    #[tokio::test]
    async fn test_batch_request() {
        let router = create_test_router();
        let mut service = JsonRpcService::new(router.clone());

        // Initialize first
        let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1.0" }
        }));
        service.call_single(init_req).await.unwrap();
        router.handle_notification(crate::protocol::McpNotification::Initialized);

        // Batch request
        let requests = vec![
            JsonRpcRequest::new(2, "tools/list").with_params(serde_json::json!({})),
            JsonRpcRequest::new(3, "tools/call").with_params(serde_json::json!({
                "name": "add",
                "arguments": { "a": 1, "b": 2 }
            })),
        ];

        let responses = service.call_batch(requests).await.unwrap();
        assert_eq!(responses.len(), 2);
    }

    #[tokio::test]
    async fn test_empty_batch_error() {
        let router = create_test_router();
        let mut service = JsonRpcService::new(router);

        let result = service.call_batch(vec![]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_jsonrpc_layer() {
        use tower::ServiceBuilder;

        let router = create_test_router();
        let router_clone = router.clone();

        // Build service using the layer via ServiceBuilder
        let mut service = ServiceBuilder::new()
            .layer(JsonRpcLayer::new())
            .service(router);

        // Initialize
        let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1.0" }
        }));
        let resp = Service::<JsonRpcRequest>::call(&mut service, init_req)
            .await
            .unwrap();
        assert!(matches!(resp, JsonRpcResponse::Result(_)));

        router_clone.handle_notification(crate::protocol::McpNotification::Initialized);

        // List tools through the layer-composed service
        let req = JsonRpcRequest::new(2, "tools/list").with_params(serde_json::json!({}));
        let resp = Service::<JsonRpcRequest>::call(&mut service, req)
            .await
            .unwrap();

        match resp {
            JsonRpcResponse::Result(r) => {
                let tools = r.result.get("tools").unwrap().as_array().unwrap();
                assert_eq!(tools.len(), 1);
            }
            JsonRpcResponse::Error(e) => panic!("Expected result, got error: {:?}", e),
            _ => panic!("unexpected response variant"),
        }
    }

    #[test]
    fn test_jsonrpc_layer_default() {
        // JsonRpcLayer implements Default
        let _layer = JsonRpcLayer::default();
    }

    #[test]
    fn test_jsonrpc_layer_clone() {
        // JsonRpcLayer implements Clone and Copy
        let layer = JsonRpcLayer::new();
        let _cloned = layer;
        let _copied = layer;
    }

    #[tokio::test]
    async fn test_invalid_jsonrpc_version() {
        let router = create_test_router();
        let mut service = JsonRpcService::new(router);

        // Request with wrong jsonrpc version
        let req = JsonRpcRequest {
            jsonrpc: "1.0".to_string(),
            id: crate::protocol::RequestId::Number(1),
            method: "ping".to_string(),
            params: None,
        };
        let resp = service.call_single(req).await.unwrap();
        match resp {
            JsonRpcResponse::Error(e) => {
                assert_eq!(e.error.code, -32600); // Invalid request
            }
            _ => panic!("Expected error for invalid jsonrpc version"),
        }
    }

    #[tokio::test]
    async fn test_unknown_method() {
        let router = create_test_router();
        let mut service = JsonRpcService::new(router.clone());

        // Initialize
        let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1.0" }
        }));
        service.call_single(init_req).await.unwrap();
        router.handle_notification(crate::protocol::McpNotification::Initialized);

        let req = JsonRpcRequest::new(2, "nonexistent/method");
        let resp = service.call_single(req).await.unwrap();
        match resp {
            JsonRpcResponse::Error(e) => {
                assert_eq!(e.error.code, -32601); // Method not found
            }
            _ => panic!("Expected error for unknown method"),
        }
    }

    #[tokio::test]
    async fn test_invalid_params() {
        let router = create_test_router();
        let mut service = JsonRpcService::new(router.clone());

        // Initialize
        let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1.0" }
        }));
        service.call_single(init_req).await.unwrap();
        router.handle_notification(crate::protocol::McpNotification::Initialized);

        // tools/call without required "name" field
        let req = JsonRpcRequest::new(2, "tools/call").with_params(serde_json::json!({
            "wrong_field": "value"
        }));
        let resp = service.call_single(req).await.unwrap();
        match resp {
            JsonRpcResponse::Error(e) => {
                assert_eq!(e.error.code, -32602); // Invalid params
            }
            _ => panic!("Expected error for invalid params"),
        }
    }

    #[tokio::test]
    async fn test_request_before_initialize() {
        let router = create_test_router();
        let mut service = JsonRpcService::new(router);

        // tools/list before initialize should fail
        let req = JsonRpcRequest::new(1, "tools/list").with_params(serde_json::json!({}));
        let resp = service.call_single(req).await.unwrap();
        match resp {
            JsonRpcResponse::Error(e) => {
                assert_eq!(e.error.code, -32600); // Invalid request (session not initialized)
            }
            _ => panic!("Expected error for request before initialize"),
        }
    }

    #[tokio::test]
    async fn test_ping_before_initialize() {
        let router = create_test_router();
        let mut service = JsonRpcService::new(router);

        // Ping should work even before initialize
        let req = JsonRpcRequest::new(1, "ping");
        let resp = service.call_single(req).await.unwrap();
        assert!(matches!(resp, JsonRpcResponse::Result(_)));
    }

    #[tokio::test]
    async fn test_call_message_single() {
        let router = create_test_router();
        let mut service = JsonRpcService::new(router);

        let msg = JsonRpcMessage::Single(JsonRpcRequest::new(1, "ping"));
        let resp = service.call_message(msg).await.unwrap();
        assert!(matches!(resp, JsonRpcResponseMessage::Single(_)));
    }

    #[tokio::test]
    async fn test_call_message_batch() {
        let router = create_test_router();
        let mut service = JsonRpcService::new(router.clone());

        // Initialize
        let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1.0" }
        }));
        service.call_single(init_req).await.unwrap();
        router.handle_notification(crate::protocol::McpNotification::Initialized);

        let msg = JsonRpcMessage::Batch(vec![
            JsonRpcRequest::new(2, "ping"),
            JsonRpcRequest::new(3, "tools/list").with_params(serde_json::json!({})),
        ]);
        let resp = service.call_message(msg).await.unwrap();
        match resp {
            JsonRpcResponseMessage::Batch(responses) => {
                assert_eq!(responses.len(), 2);
            }
            _ => panic!("Expected batch response"),
        }
    }

    #[tokio::test]
    async fn test_call_message_empty_batch() {
        let router = create_test_router();
        let mut service = JsonRpcService::new(router);

        // call_message delegates to call_batch which returns Err for empty batch
        let msg = JsonRpcMessage::Batch(vec![]);
        let result = service.call_message(msg).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_extensions_bridging() {
        let router = create_test_router();

        #[derive(Debug, Clone)]
        #[allow(dead_code)]
        struct TestClaim(String);

        let mut ext = Extensions::new();
        ext.insert(TestClaim("admin".to_string()));

        let mut service = JsonRpcService::new(router).with_extensions(ext);

        // Ping should work -- extensions are injected into RouterRequest
        let req = JsonRpcRequest::new(1, "ping");
        let resp = service.call_single(req).await.unwrap();
        assert!(matches!(resp, JsonRpcResponse::Result(_)));
    }

    #[tokio::test]
    async fn test_batch_with_mixed_valid_invalid() {
        let router = create_test_router();
        let mut service = JsonRpcService::new(router.clone());

        // Initialize
        let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1.0" }
        }));
        service.call_single(init_req).await.unwrap();
        router.handle_notification(crate::protocol::McpNotification::Initialized);

        // Batch with one valid and one invalid request
        let requests = vec![
            JsonRpcRequest::new(2, "ping"),
            JsonRpcRequest::new(3, "nonexistent/method"),
        ];
        let responses = service.call_batch(requests).await.unwrap();
        assert_eq!(responses.len(), 2);

        // First should succeed (ping)
        assert!(matches!(&responses[0], JsonRpcResponse::Result(_)));
        // Second should be an error (method not found)
        assert!(matches!(&responses[1], JsonRpcResponse::Error(_)));
    }
}
