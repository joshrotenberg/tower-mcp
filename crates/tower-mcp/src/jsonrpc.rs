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
    JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, JsonRpcResponseMessage, McpRequest, ResultType,
};
use crate::router::{Extensions, RouterRequest, RouterResponse};
use crate::{ProtocolSupport, ProtocolSupportError};

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
    protocol_support: ProtocolSupport,
}

impl<S> JsonRpcService<S> {
    /// Create a new JSON-RPC service wrapping the given inner service
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            extensions: Extensions::new(),
            protocol_support: ProtocolSupport::default(),
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

    /// Set the exact protocol versions this service accepts and advertises.
    ///
    /// The default enables every protocol implementation compiled into
    /// `tower-mcp`. Transports expose the same policy so applications can
    /// narrow support per server instance.
    pub fn protocol_support(mut self, support: ProtocolSupport) -> Self {
        self.protocol_support = support;
        self
    }

    /// Construct and set an exact runtime protocol-version allow-list.
    pub fn protocol_versions<I, V>(
        mut self,
        versions: I,
    ) -> std::result::Result<Self, ProtocolSupportError>
    where
        I: IntoIterator<Item = V>,
        V: Into<String>,
    {
        self.protocol_support = ProtocolSupport::try_new(versions)?;
        Ok(self)
    }

    pub(crate) fn configured_protocol_support(&self) -> &ProtocolSupport {
        &self.protocol_support
    }

    /// Validate the lifecycle metadata on a request without dispatching it.
    ///
    /// Long-lived transport-owned requests such as `subscriptions/listen`
    /// need to be accepted by the binding before they reach the ordinary
    /// request/response router. Keeping this validation here ensures those
    /// transport paths honor the same runtime protocol allow-list and modern
    /// metadata rules as [`Self::call_single`].
    #[cfg(feature = "stateless")]
    pub(crate) fn validate_request_protocol(
        &self,
        req: &JsonRpcRequest,
    ) -> std::result::Result<Option<String>, JsonRpcError> {
        req.validate()?;
        let mut extensions = self.extensions.clone();
        let protocol_support = extensions
            .get::<ProtocolSupport>()
            .cloned()
            .unwrap_or_else(|| self.protocol_support.clone());
        prepare_modern_request(req, &mut extensions, &protocol_support)
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
        process_single_request(
            self.inner.clone(),
            req,
            self.extensions.clone(),
            self.protocol_support.clone(),
        )
        .await
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
                let protocol_support = self.protocol_support.clone();
                let req_id = req.id.clone();
                async move {
                    match process_single_request(inner, req, extensions, protocol_support).await {
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
            protocol_support: self.protocol_support.clone(),
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
        let protocol_support = self.protocol_support.clone();
        Box::pin(process_single_request(
            inner,
            req,
            extensions,
            protocol_support,
        ))
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
        let protocol_support = self.protocol_support.clone();
        Box::pin(async move {
            match msg {
                JsonRpcMessage::Single(req) => {
                    let response =
                        process_single_request(inner, req, extensions, protocol_support).await?;
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
                            let protocol_support = protocol_support.clone();
                            let req_id = req.id.clone();
                            async move {
                                match process_single_request(
                                    inner,
                                    req,
                                    extensions,
                                    protocol_support,
                                )
                                .await
                                {
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
    mut extensions: Extensions,
    configured_protocol_support: ProtocolSupport,
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

    let method = req.method.clone();
    #[cfg(feature = "stateless")]
    let request_id = req.id.clone();
    let protocol_support = extensions
        .get::<ProtocolSupport>()
        .cloned()
        .unwrap_or(configured_protocol_support);
    extensions.insert(protocol_support.clone());

    #[cfg(feature = "stateless")]
    let protocol_version = match prepare_modern_request(&req, &mut extensions, &protocol_support) {
        Ok(version) => version,
        Err(error) => return Ok(JsonRpcResponse::error(Some(request_id), error)),
    };
    #[cfg(not(feature = "stateless"))]
    let protocol_version: Option<String> = None;

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
    let mut response = response.into_jsonrpc();
    if let Some(version) = protocol_version.as_deref() {
        apply_protocol_result_fields(&mut response, &method, version);
    }
    Ok(response)
}

/// Validate a request using the final per-request metadata lifecycle.
///
/// The protocol-version key is the era discriminator on transports without
/// headers (stdio and custom bindings). Legacy initialize traffic can coexist
/// on the same connection because requests without that key retain the
/// sessionful path.
#[cfg(feature = "stateless")]
fn prepare_modern_request(
    req: &JsonRpcRequest,
    extensions: &mut Extensions,
    protocol_support: &ProtocolSupport,
) -> std::result::Result<Option<String>, JsonRpcError> {
    let Some(params) = req.params.as_ref() else {
        return Ok(None);
    };
    let Some(meta_value) = params.as_object().and_then(|params| params.get("_meta")) else {
        return Ok(None);
    };
    let claims_modern = meta_value
        .as_object()
        .is_some_and(|meta| meta.contains_key("io.modelcontextprotocol/protocolVersion"));
    if !claims_modern {
        return Ok(None);
    }

    crate::protocol::validate_meta_object(meta_value)
        .map_err(|error| JsonRpcError::invalid_params(error.to_string()))?;
    let meta_object = meta_value
        .as_object()
        .expect("validate_meta_object accepted a JSON object");
    let protocol_version = meta_object
        .get("io.modelcontextprotocol/protocolVersion")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            JsonRpcError::invalid_params(
                "Missing or invalid _meta.io.modelcontextprotocol/protocolVersion",
            )
        })?;
    let client_capabilities = meta_object
        .get("io.modelcontextprotocol/clientCapabilities")
        .ok_or_else(|| {
            JsonRpcError::invalid_params("Missing _meta.io.modelcontextprotocol/clientCapabilities")
        })?;
    if !client_capabilities.is_object()
        || serde_json::from_value::<crate::protocol::ClientCapabilities>(
            client_capabilities.clone(),
        )
        .is_err()
    {
        return Err(JsonRpcError::invalid_params(
            "Invalid _meta.io.modelcontextprotocol/clientCapabilities",
        ));
    }
    if !protocol_support.contains(protocol_version) {
        return Err(JsonRpcError::unsupported_protocol_version(
            protocol_version,
            protocol_support.versions().iter().map(String::as_str),
        ));
    }
    if protocol_version == crate::protocol::EXPERIMENTAL_PROTOCOL_VERSION
        && is_removed_modern_method(&req.method)
    {
        return Err(JsonRpcError::method_not_found(&req.method));
    }

    let meta: crate::stateless::StatelessRequestMeta =
        serde_json::from_value(meta_value.clone())
            .map_err(|error| JsonRpcError::invalid_params(error.to_string()))?;
    extensions.insert(meta);
    Ok(Some(protocol_version.to_string()))
}

/// Methods present in legacy protocol unions but removed from the final core.
#[cfg(feature = "stateless")]
fn is_removed_modern_method(method: &str) -> bool {
    matches!(
        method,
        "initialize"
            | "notifications/initialized"
            | "ping"
            | "logging/setLevel"
            | "resources/subscribe"
            | "resources/unsubscribe"
            | "notifications/roots/list_changed"
    )
}

/// Fill the required 2026-07-28 result envelope immediately before it reaches
/// a JSON-RPC transport, while preserving legacy public result types.
pub(crate) fn apply_protocol_result_fields(
    response: &mut JsonRpcResponse,
    method: &str,
    protocol_version: &str,
) {
    if protocol_version != crate::protocol::EXPERIMENTAL_PROTOCOL_VERSION {
        return;
    }

    let JsonRpcResponse::Result(result) = response else {
        return;
    };
    ResultType::Complete.stamp_into(&mut result.result, protocol_version);

    if !is_cacheable_result_method(method) {
        return;
    }
    let Some(object) = result.result.as_object_mut() else {
        return;
    };
    object
        .entry("ttlMs")
        .or_insert_with(|| serde_json::Value::Number(0.into()));
    object
        .entry("cacheScope")
        .or_insert_with(|| serde_json::Value::String("private".to_string()));
}

fn is_cacheable_result_method(method: &str) -> bool {
    matches!(
        method,
        "server/discover"
            | "tools/list"
            | "prompts/list"
            | "resources/list"
            | "resources/read"
            | "resources/templates/list"
    )
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

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn modern_batch_metadata_is_isolated_per_request() {
        let router = create_test_router();
        let mut service = JsonRpcService::new(router);
        let final_request = JsonRpcRequest::new(1, "tools/list").with_params(serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion":
                    crate::protocol::EXPERIMENTAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }));
        let legacy_request =
            JsonRpcRequest::new(2, "tools/list").with_params(serde_json::json!({}));

        let responses = service
            .call_batch(vec![final_request, legacy_request])
            .await
            .unwrap();
        let JsonRpcResponse::Result(final_response) = &responses[0] else {
            panic!("final request should bypass legacy session state");
        };
        assert_eq!(final_response.result["resultType"], "complete");

        let JsonRpcResponse::Error(legacy_response) = &responses[1] else {
            panic!("legacy request must not inherit final request metadata");
        };
        assert_eq!(legacy_response.error.code, -32600);
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn modern_request_requires_client_capabilities() {
        let router = create_test_router();
        let mut service = JsonRpcService::new(router);
        let request = JsonRpcRequest::new(1, "server/discover").with_params(serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion":
                    crate::protocol::EXPERIMENTAL_PROTOCOL_VERSION
            }
        }));

        let response = service.call_single(request).await.unwrap();
        let JsonRpcResponse::Error(response) = response else {
            panic!("missing clientCapabilities must be rejected");
        };
        assert_eq!(response.error.code, -32602);
        assert!(response.error.message.contains("clientCapabilities"));
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn final_only_policy_never_negotiates_final_via_initialize() {
        let router = create_test_router();
        let mut service = JsonRpcService::new(router).protocol_support(
            ProtocolSupport::try_new([crate::protocol::EXPERIMENTAL_PROTOCOL_VERSION]).unwrap(),
        );
        let request = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "legacy-client", "version": "1.0.0"}
        }));

        let response = service.call_single(request).await.unwrap();
        let JsonRpcResponse::Error(response) = response else {
            panic!("final-only policy must reject the removed initialize lifecycle");
        };
        assert_eq!(response.error.code, -32022);
        assert_eq!(
            response.error.data.unwrap()["supported"],
            serde_json::json!([crate::protocol::EXPERIMENTAL_PROTOCOL_VERSION])
        );
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
