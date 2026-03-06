//! Tool call audit logging middleware.
//!
//! This module provides [`ToolCallLoggingLayer`], a Tower middleware that emits
//! structured [`tracing`] events specifically for `tools/call` requests. Unlike
//! [`McpTracingLayer`](super::McpTracingLayer) which traces all MCP requests,
//! this layer focuses on tool invocations and provides richer audit information.
//!
//! # Logged Information
//!
//! For each tool call, the layer emits a single event after completion with:
//! - **tool**: The tool name
//! - **duration_ms**: Execution time in milliseconds
//! - **status**: One of `"success"`, `"error"`, or `"denied"`
//! - **error_code** / **error_message**: Present on error responses
//!
//! All events use tracing target `mcp::tools` for easy filtering.
//! Non-`CallTool` requests pass through unchanged with no overhead.
//!
//! # Example
//!
//! ```rust,ignore
//! use tower_mcp::{McpRouter, StdioTransport};
//! use tower_mcp::middleware::ToolCallLoggingLayer;
//!
//! let router = McpRouter::new().server_info("my-server", "1.0.0");
//!
//! let mut transport = StdioTransport::new(router)
//!     .layer(ToolCallLoggingLayer::new());
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

/// JSON-RPC error code for "invalid params", which may indicate a denied tool call.
const JSONRPC_INVALID_PARAMS: i32 = -32602;

/// Tower layer that adds audit logging for tool call requests.
///
/// This layer intercepts `tools/call` requests and emits structured
/// [`tracing`] events with the tool name, execution duration, and result
/// status. Non-tool-call requests pass through with zero overhead.
///
/// Events are emitted with tracing target `mcp::tools`, which can be used
/// for filtering in your tracing subscriber configuration.
///
/// # Example
///
/// ```rust,ignore
/// use tower_mcp::{McpRouter, StdioTransport};
/// use tower_mcp::middleware::ToolCallLoggingLayer;
///
/// let router = McpRouter::new().server_info("my-server", "1.0.0");
///
/// let mut transport = StdioTransport::new(router)
///     .layer(ToolCallLoggingLayer::new());
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ToolCallLoggingLayer {
    level: Level,
}

impl Default for ToolCallLoggingLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCallLoggingLayer {
    /// Create a new tool call logging layer with default settings (INFO level).
    pub fn new() -> Self {
        Self { level: Level::INFO }
    }

    /// Set the log level for tool call events.
    ///
    /// Default is `INFO`.
    pub fn level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }
}

impl<S> Layer<S> for ToolCallLoggingLayer {
    type Service = ToolCallLoggingService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ToolCallLoggingService {
            inner,
            level: self.level,
        }
    }
}

/// Tower service that logs tool call requests.
///
/// Created by [`ToolCallLoggingLayer`]. See the layer documentation for details.
#[derive(Debug, Clone)]
pub struct ToolCallLoggingService<S> {
    inner: S,
    level: Level,
}

impl<S> Service<RouterRequest> for ToolCallLoggingService<S>
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
        // Only intercept CallTool requests; pass everything else through directly.
        let tool_name = match &req.inner {
            McpRequest::CallTool(params) => params.name.clone(),
            _ => {
                let fut = self.inner.call(req);
                return Box::pin(fut);
            }
        };

        // Extract annotation hints if the transport injected them.
        let read_only = req
            .extensions
            .get::<ToolAnnotationsMap>()
            .map(|m| m.is_read_only(&tool_name));
        let destructive = req
            .extensions
            .get::<ToolAnnotationsMap>()
            .map(|m| m.is_destructive(&tool_name));

        let start = Instant::now();
        let fut = self.inner.call(req);
        let level = self.level;

        Box::pin(async move {
            let result = fut.await;
            let duration_ms = start.elapsed().as_secs_f64() * 1000.0;

            if let Ok(response) = &result {
                match &response.inner {
                    Ok(_) => {
                        log_tool_call(
                            level,
                            &tool_name,
                            duration_ms,
                            "success",
                            None,
                            read_only,
                            destructive,
                        );
                    }
                    Err(err) => {
                        let status = if err.code == JSONRPC_INVALID_PARAMS {
                            "denied"
                        } else {
                            "error"
                        };
                        log_tool_call(
                            level,
                            &tool_name,
                            duration_ms,
                            status,
                            Some((err.code, &err.message)),
                            read_only,
                            destructive,
                        );
                    }
                }
            }

            result
        })
    }
}

/// Emit a structured tracing event for a tool call.
fn log_tool_call(
    level: Level,
    tool: &str,
    duration_ms: f64,
    status: &str,
    error: Option<(i32, &str)>,
    read_only: Option<bool>,
    destructive: Option<bool>,
) {
    match (level, error) {
        (Level::TRACE, None) => {
            tracing::trace!(target: "mcp::tools", tool, duration_ms, status, ?read_only, ?destructive, "tool call completed")
        }
        (Level::TRACE, Some((code, message))) => {
            tracing::trace!(target: "mcp::tools", tool, duration_ms, status, error_code = code, error_message = message, ?read_only, ?destructive, "tool call completed")
        }
        (Level::DEBUG, None) => {
            tracing::debug!(target: "mcp::tools", tool, duration_ms, status, ?read_only, ?destructive, "tool call completed")
        }
        (Level::DEBUG, Some((code, message))) => {
            tracing::debug!(target: "mcp::tools", tool, duration_ms, status, error_code = code, error_message = message, ?read_only, ?destructive, "tool call completed")
        }
        (Level::INFO, None) => {
            tracing::info!(target: "mcp::tools", tool, duration_ms, status, ?read_only, ?destructive, "tool call completed")
        }
        (Level::INFO, Some((code, message))) => {
            tracing::info!(target: "mcp::tools", tool, duration_ms, status, error_code = code, error_message = message, ?read_only, ?destructive, "tool call completed")
        }
        (Level::WARN, None) => {
            tracing::warn!(target: "mcp::tools", tool, duration_ms, status, ?read_only, ?destructive, "tool call completed")
        }
        (Level::WARN, Some((code, message))) => {
            tracing::warn!(target: "mcp::tools", tool, duration_ms, status, error_code = code, error_message = message, ?read_only, ?destructive, "tool call completed")
        }
        (Level::ERROR, None) => {
            tracing::error!(target: "mcp::tools", tool, duration_ms, status, ?read_only, ?destructive, "tool call completed")
        }
        (Level::ERROR, Some((code, message))) => {
            tracing::error!(target: "mcp::tools", tool, duration_ms, status, error_code = code, error_message = message, ?read_only, ?destructive, "tool call completed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{CallToolParams, RequestId};
    use crate::router::Extensions;

    #[test]
    fn test_layer_creation() {
        let layer = ToolCallLoggingLayer::new();
        assert_eq!(layer.level, Level::INFO);
    }

    #[test]
    fn test_layer_with_custom_level() {
        let layer = ToolCallLoggingLayer::new().level(Level::DEBUG);
        assert_eq!(layer.level, Level::DEBUG);
    }

    #[test]
    fn test_layer_default() {
        let layer = ToolCallLoggingLayer::default();
        assert_eq!(layer.level, Level::INFO);
    }

    #[tokio::test]
    async fn test_non_tool_call_passthrough() {
        let router = crate::McpRouter::new().server_info("test", "1.0.0");
        let layer = ToolCallLoggingLayer::new();
        let mut service = layer.layer(router);

        // Ping request should pass through without tool call logging
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
    async fn test_tool_call_logging() {
        let tool = crate::ToolBuilder::new("test_tool")
            .description("A test tool")
            .handler(|_: serde_json::Value| async move { Ok(crate::CallToolResult::text("done")) })
            .build();

        let router = crate::McpRouter::new()
            .server_info("test", "1.0.0")
            .tool(tool);
        let layer = ToolCallLoggingLayer::new();
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

        // The service should process the request and return a response.
        // The inner result may be an error (session not initialized) but
        // the logging layer should handle it either way.
        let result = Service::call(&mut service, req).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_tool_call_error_logging() {
        // Call a tool that doesn't exist to get an error response
        let router = crate::McpRouter::new().server_info("test", "1.0.0");
        let layer = ToolCallLoggingLayer::new();
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
        // The response should be an error (tool not found)
        assert!(result.unwrap().inner.is_err());
    }
}
