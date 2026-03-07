//! MCP audit logging middleware.
//!
//! This module provides [`AuditLayer`], a Tower middleware that emits structured
//! audit events for all MCP requests using the [`tracing`] crate. Unlike
//! [`McpTracingLayer`](super::McpTracingLayer) which focuses on request tracing
//! with spans, or [`ToolCallLoggingLayer`](super::ToolCallLoggingLayer) which
//! focuses only on tool calls, `AuditLayer` emits a single, well-structured
//! audit event per request designed for compliance and security monitoring.
//!
//! # Audit Event Fields
//!
//! Every audit event includes:
//! - **method**: The MCP method (e.g., `tools/call`, `resources/read`)
//! - **request_id**: The JSON-RPC request ID
//! - **duration_ms**: Request processing time in milliseconds
//! - **status**: `"success"`, `"error"`, or `"denied"`
//!
//! Operation-specific fields:
//! - **tool**: Tool name (for `tools/call`)
//! - **resource_uri**: Resource URI (for `resources/read`)
//! - **prompt**: Prompt name (for `prompts/get`)
//! - **error_code** / **error_message**: Present on error responses
//! - **read_only** / **destructive**: Tool annotation hints (when available)
//!
//! All events use tracing target `mcp::audit` for easy filtering and routing
//! to dedicated audit log sinks.
//!
//! # Example
//!
//! ```rust,ignore
//! use tower_mcp::middleware::AuditLayer;
//!
//! // Route audit events to a file via tracing-subscriber
//! let transport = StdioTransport::new(router)
//!     .layer(AuditLayer::new());
//!
//! // Filter audit events in subscriber config:
//! // RUST_LOG="mcp::audit=info"
//! ```

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use tower::Layer;
use tower_service::Service;
use tracing::Level;

use crate::protocol::McpRequest;
use crate::router::{RouterRequest, RouterResponse, ToolAnnotationsMap};

/// Tower layer that adds structured audit logging to all MCP requests.
///
/// This layer emits one structured [`tracing`] event per request at completion,
/// using target `mcp::audit`. Events contain the method, operation details,
/// duration, status, and tool annotation hints when available.
///
/// # Example
///
/// ```rust,ignore
/// use tower_mcp::middleware::AuditLayer;
///
/// let transport = HttpTransport::new(router)
///     .layer(AuditLayer::new());
/// ```
#[derive(Debug, Clone, Copy)]
pub struct AuditLayer {
    level: Level,
}

impl Default for AuditLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditLayer {
    /// Create a new audit layer with default settings (INFO level).
    pub fn new() -> Self {
        Self { level: Level::INFO }
    }

    /// Set the log level for audit events.
    ///
    /// Default is `INFO`.
    pub fn level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }
}

impl<S> Layer<S> for AuditLayer {
    type Service = AuditService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuditService {
            inner,
            level: self.level,
        }
    }
}

/// Tower service that emits structured audit events for MCP requests.
///
/// Created by [`AuditLayer`]. See the layer documentation for details.
#[derive(Debug, Clone)]
pub struct AuditService<S> {
    inner: S,
    level: Level,
}

/// Audit-relevant details extracted from a request before forwarding.
struct AuditInfo {
    method: String,
    request_id: String,
    tool: Option<String>,
    resource_uri: Option<String>,
    prompt: Option<String>,
    read_only: Option<bool>,
    destructive: Option<bool>,
}

impl AuditInfo {
    fn extract(req: &RouterRequest) -> Self {
        let method = req.inner.method_name().to_string();
        let request_id = format!("{:?}", req.id);

        let mut info = Self {
            method,
            request_id,
            tool: None,
            resource_uri: None,
            prompt: None,
            read_only: None,
            destructive: None,
        };

        match &req.inner {
            McpRequest::CallTool(params) => {
                info.tool = Some(params.name.clone());

                if let Some(annotations) = req.extensions.get::<ToolAnnotationsMap>() {
                    info.read_only = Some(annotations.is_read_only(&params.name));
                    info.destructive = Some(annotations.is_destructive(&params.name));
                }
            }
            McpRequest::ReadResource(params) => {
                info.resource_uri = Some(params.uri.clone());
            }
            McpRequest::GetPrompt(params) => {
                info.prompt = Some(params.name.clone());
            }
            McpRequest::SubscribeResource(params) => {
                info.resource_uri = Some(params.uri.clone());
            }
            McpRequest::UnsubscribeResource(params) => {
                info.resource_uri = Some(params.uri.clone());
            }
            _ => {}
        }

        info
    }
}

/// JSON-RPC error code for "invalid params", which may indicate a denied request.
const JSONRPC_INVALID_PARAMS: i32 = -32602;

impl<S> Service<RouterRequest> for AuditService<S>
where
    S: Service<RouterRequest, Response = RouterResponse> + Clone + Send + 'static,
    S::Error: Send,
    S::Future: Send,
{
    type Response = RouterResponse;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let info = AuditInfo::extract(&req);
        let start = Instant::now();
        let fut = self.inner.call(req);
        let level = self.level;

        Box::pin(async move {
            let result = fut.await;
            let duration_ms = start.elapsed().as_secs_f64() * 1000.0;

            if let Ok(response) = &result {
                let (status, error) = match &response.inner {
                    Ok(_) => ("success", None),
                    Err(err) => {
                        let s = if err.code == JSONRPC_INVALID_PARAMS {
                            "denied"
                        } else {
                            "error"
                        };
                        (s, Some((err.code, err.message.as_str())))
                    }
                };

                emit_audit_event(level, &info, duration_ms, status, error);
            }

            result
        })
    }
}

/// Emit a structured audit event at the configured level.
///
/// Uses tracing target `mcp::audit` so events can be routed independently
/// from other application logs.
fn emit_audit_event(
    level: Level,
    info: &AuditInfo,
    duration_ms: f64,
    status: &str,
    error: Option<(i32, &str)>,
) {
    let method = info.method.as_str();
    let request_id = info.request_id.as_str();
    let tool = info.tool.as_deref();
    let resource_uri = info.resource_uri.as_deref();
    let prompt = info.prompt.as_deref();
    let read_only = info.read_only;
    let destructive = info.destructive;

    match (level, error) {
        (Level::TRACE, None) => {
            tracing::trace!(target: "mcp::audit", method, request_id, ?tool, ?resource_uri, ?prompt, duration_ms, status, ?read_only, ?destructive, "audit")
        }
        (Level::TRACE, Some((code, msg))) => {
            tracing::trace!(target: "mcp::audit", method, request_id, ?tool, ?resource_uri, ?prompt, duration_ms, status, error_code = code, error_message = msg, ?read_only, ?destructive, "audit")
        }
        (Level::DEBUG, None) => {
            tracing::debug!(target: "mcp::audit", method, request_id, ?tool, ?resource_uri, ?prompt, duration_ms, status, ?read_only, ?destructive, "audit")
        }
        (Level::DEBUG, Some((code, msg))) => {
            tracing::debug!(target: "mcp::audit", method, request_id, ?tool, ?resource_uri, ?prompt, duration_ms, status, error_code = code, error_message = msg, ?read_only, ?destructive, "audit")
        }
        (Level::INFO, None) => {
            tracing::info!(target: "mcp::audit", method, request_id, ?tool, ?resource_uri, ?prompt, duration_ms, status, ?read_only, ?destructive, "audit")
        }
        (Level::INFO, Some((code, msg))) => {
            tracing::info!(target: "mcp::audit", method, request_id, ?tool, ?resource_uri, ?prompt, duration_ms, status, error_code = code, error_message = msg, ?read_only, ?destructive, "audit")
        }
        (Level::WARN, None) => {
            tracing::warn!(target: "mcp::audit", method, request_id, ?tool, ?resource_uri, ?prompt, duration_ms, status, ?read_only, ?destructive, "audit")
        }
        (Level::WARN, Some((code, msg))) => {
            tracing::warn!(target: "mcp::audit", method, request_id, ?tool, ?resource_uri, ?prompt, duration_ms, status, error_code = code, error_message = msg, ?read_only, ?destructive, "audit")
        }
        (Level::ERROR, None) => {
            tracing::error!(target: "mcp::audit", method, request_id, ?tool, ?resource_uri, ?prompt, duration_ms, status, ?read_only, ?destructive, "audit")
        }
        (Level::ERROR, Some((code, msg))) => {
            tracing::error!(target: "mcp::audit", method, request_id, ?tool, ?resource_uri, ?prompt, duration_ms, status, error_code = code, error_message = msg, ?read_only, ?destructive, "audit")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{CallToolParams, GetPromptParams, ReadResourceParams, RequestId};
    use crate::router::Extensions;
    use std::collections::HashMap;

    #[test]
    fn test_layer_creation() {
        let layer = AuditLayer::new();
        assert_eq!(layer.level, Level::INFO);
    }

    #[test]
    fn test_layer_with_custom_level() {
        let layer = AuditLayer::new().level(Level::DEBUG);
        assert_eq!(layer.level, Level::DEBUG);
    }

    #[test]
    fn test_layer_default() {
        let layer = AuditLayer::default();
        assert_eq!(layer.level, Level::INFO);
    }

    #[test]
    fn test_audit_info_tool_call() {
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "my_tool".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };

        let info = AuditInfo::extract(&req);
        assert_eq!(info.method, "tools/call");
        assert_eq!(info.tool, Some("my_tool".to_string()));
        assert!(info.resource_uri.is_none());
        assert!(info.prompt.is_none());
    }

    #[test]
    fn test_audit_info_resource_read() {
        let req = RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::ReadResource(ReadResourceParams {
                uri: "file:///test.txt".to_string(),
                meta: None,
            }),
            extensions: Extensions::new(),
        };

        let info = AuditInfo::extract(&req);
        assert_eq!(info.method, "resources/read");
        assert!(info.tool.is_none());
        assert_eq!(info.resource_uri, Some("file:///test.txt".to_string()));
    }

    #[test]
    fn test_audit_info_prompt_get() {
        let req = RouterRequest {
            id: RequestId::Number(3),
            inner: McpRequest::GetPrompt(GetPromptParams {
                name: "review".to_string(),
                arguments: HashMap::new(),
                meta: None,
            }),
            extensions: Extensions::new(),
        };

        let info = AuditInfo::extract(&req);
        assert_eq!(info.method, "prompts/get");
        assert!(info.tool.is_none());
        assert_eq!(info.prompt, Some("review".to_string()));
    }

    #[test]
    fn test_audit_info_ping() {
        let req = RouterRequest {
            id: RequestId::Number(4),
            inner: McpRequest::Ping,
            extensions: Extensions::new(),
        };

        let info = AuditInfo::extract(&req);
        assert_eq!(info.method, "ping");
        assert!(info.tool.is_none());
        assert!(info.resource_uri.is_none());
        assert!(info.prompt.is_none());
    }

    #[tokio::test]
    async fn test_passthrough() {
        let router = crate::McpRouter::new().server_info("test", "1.0.0");
        let layer = AuditLayer::new();
        let mut service = layer.layer(router);

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::Ping,
            extensions: Extensions::new(),
        };

        let result = Service::call(&mut service, req).await;
        assert!(result.is_ok());
        assert!(result.unwrap().inner.is_ok());
    }

    #[tokio::test]
    async fn test_tool_call_audit() {
        let tool = crate::ToolBuilder::new("test_tool")
            .description("A test tool")
            .handler(|_: serde_json::Value| async move { Ok(crate::CallToolResult::text("done")) })
            .build();

        let router = crate::McpRouter::new()
            .server_info("test", "1.0.0")
            .tool(tool);
        let layer = AuditLayer::new();
        let mut service = layer.layer(router);

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "test_tool".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };

        let result = Service::call(&mut service, req).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_error_audit() {
        let router = crate::McpRouter::new().server_info("test", "1.0.0");
        let layer = AuditLayer::new();
        let mut service = layer.layer(router);

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "nonexistent".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };

        let result = Service::call(&mut service, req).await;
        assert!(result.is_ok());
        assert!(result.unwrap().inner.is_err());
    }
}
