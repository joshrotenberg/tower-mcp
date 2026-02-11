//! MCP request tracing middleware.
//!
//! This module provides [`McpTracingLayer`], a Tower middleware that logs
//! structured information about MCP requests using the [`tracing`] crate.
//!
//! # Example
//!
//! ```rust,ignore
//! use tower_mcp::{McpRouter, McpTracingLayer, HttpTransport};
//!
//! let router = McpRouter::new().server_info("my-server", "1.0.0");
//!
//! // Add tracing to all MCP requests
//! let transport = HttpTransport::new(router)
//!     .layer(McpTracingLayer::new());
//! ```
//!
//! # Logged Information
//!
//! For each request, the layer logs:
//! - Request method (e.g., `tools/call`, `resources/read`)
//! - Request ID
//! - Operation-specific details:
//!   - Tool calls: tool name
//!   - Resource reads: resource URI
//!   - Prompt gets: prompt name
//! - Request duration
//! - Response status (success or error code)
//!
//! # Log Levels
//!
//! - `INFO`: Request start and completion
//! - `DEBUG`: Detailed request/response information
//! - `WARN`: Error responses

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use tower::Layer;
use tower_service::Service;
use tracing::{Instrument, Level, Span};

use crate::protocol::McpRequest;
use crate::router::{RouterRequest, RouterResponse};

/// Tower layer that adds structured tracing to MCP requests.
///
/// This layer wraps a service and logs information about each request
/// using the [`tracing`] crate. It's designed to work with tower-mcp's
/// `RouterRequest`/`RouterResponse` types.
///
/// # Example
///
/// ```rust,ignore
/// use tower_mcp::{McpRouter, McpTracingLayer, HttpTransport};
///
/// let router = McpRouter::new().server_info("my-server", "1.0.0");
///
/// // Apply at the transport level for all requests
/// let transport = HttpTransport::new(router)
///     .layer(McpTracingLayer::new());
///
/// // Or apply to specific tools via ToolBuilder
/// let tool = ToolBuilder::new("search")
///     .handler(|input: SearchInput| async move { ... })
///     .layer(McpTracingLayer::new())
///     .build();
/// ```
#[derive(Debug, Clone, Copy)]
pub struct McpTracingLayer {
    level: Level,
}

impl Default for McpTracingLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl McpTracingLayer {
    /// Create a new tracing layer with default settings (INFO level).
    pub fn new() -> Self {
        Self { level: Level::INFO }
    }

    /// Set the log level for request/response logging.
    ///
    /// Default is `INFO`.
    pub fn level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }
}

impl<S> Layer<S> for McpTracingLayer {
    type Service = McpTracingService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        McpTracingService {
            inner,
            level: self.level,
        }
    }
}

/// Tower service that adds tracing to MCP requests.
///
/// Created by [`McpTracingLayer`].
#[derive(Debug, Clone)]
pub struct McpTracingService<S> {
    inner: S,
    level: Level,
}

impl<S> Service<RouterRequest> for McpTracingService<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let method = req.inner.method_name().to_string();
        let request_id = format!("{:?}", req.id);

        // Extract operation-specific details
        let (operation_name, operation_target) = extract_operation_details(&req.inner);

        // Create the span based on the configured level
        let span = create_span(
            self.level,
            &method,
            &request_id,
            operation_name,
            operation_target,
        );

        let start = Instant::now();
        let fut = self.inner.call(req);
        let level = self.level;

        Box::pin(
            async move {
                let result = fut.await;
                let duration = start.elapsed();

                match &result {
                    Ok(response) => {
                        let duration_ms = duration.as_secs_f64() * 1000.0;
                        match &response.inner {
                            Ok(_) => {
                                log_success(level, &method, duration_ms);
                            }
                            Err(err) => {
                                tracing::warn!(
                                    method = %method,
                                    error_code = err.code,
                                    error_message = %err.message,
                                    duration_ms = duration_ms,
                                    "MCP request failed"
                                );
                            }
                        }
                    }
                    Err(_) => {
                        // Infallible, but handle for completeness
                        tracing::error!(method = %method, "MCP request error (infallible)");
                    }
                }

                result
            }
            .instrument(span),
        )
    }
}

/// Extract operation-specific name and target from the request.
fn extract_operation_details(req: &McpRequest) -> (Option<&'static str>, Option<String>) {
    match req {
        McpRequest::CallTool(params) => (Some("tool"), Some(params.name.clone())),
        McpRequest::ReadResource(params) => (Some("resource"), Some(params.uri.clone())),
        McpRequest::GetPrompt(params) => (Some("prompt"), Some(params.name.clone())),
        McpRequest::ListTools(_) => (Some("list"), Some("tools".to_string())),
        McpRequest::ListResources(_) => (Some("list"), Some("resources".to_string())),
        McpRequest::ListResourceTemplates(_) => {
            (Some("list"), Some("resource_templates".to_string()))
        }
        McpRequest::ListPrompts(_) => (Some("list"), Some("prompts".to_string())),
        McpRequest::SubscribeResource(params) => (Some("subscribe"), Some(params.uri.clone())),
        McpRequest::UnsubscribeResource(params) => (Some("unsubscribe"), Some(params.uri.clone())),
        McpRequest::ListTasks(_) => (Some("list"), Some("tasks".to_string())),
        McpRequest::GetTaskInfo(params) => (Some("task"), Some(params.task_id.clone())),
        McpRequest::GetTaskResult(params) => (Some("task_result"), Some(params.task_id.clone())),
        McpRequest::CancelTask(params) => (Some("cancel"), Some(params.task_id.clone())),
        McpRequest::Complete(params) => {
            let ref_type = match &params.reference {
                crate::protocol::CompletionReference::Resource { uri } => {
                    format!("resource:{}", uri)
                }
                crate::protocol::CompletionReference::Prompt { name } => {
                    format!("prompt:{}", name)
                }
            };
            (Some("complete"), Some(ref_type))
        }
        McpRequest::SetLoggingLevel(params) => {
            (Some("logging"), Some(format!("{:?}", params.level)))
        }
        McpRequest::Initialize(_) => (Some("init"), None),
        McpRequest::Ping => (Some("ping"), None),
        McpRequest::Unknown { method, .. } => (Some("unknown"), Some(method.clone())),
    }
}

/// Create a tracing span with the appropriate level.
fn create_span(
    level: Level,
    method: &str,
    request_id: &str,
    operation_name: Option<&str>,
    operation_target: Option<String>,
) -> Span {
    match level {
        Level::TRACE => tracing::trace_span!(
            "mcp_request",
            method = %method,
            request_id = %request_id,
            operation = operation_name,
            target = operation_target.as_deref(),
        ),
        Level::DEBUG => tracing::debug_span!(
            "mcp_request",
            method = %method,
            request_id = %request_id,
            operation = operation_name,
            target = operation_target.as_deref(),
        ),
        Level::INFO => tracing::info_span!(
            "mcp_request",
            method = %method,
            request_id = %request_id,
            operation = operation_name,
            target = operation_target.as_deref(),
        ),
        Level::WARN => tracing::warn_span!(
            "mcp_request",
            method = %method,
            request_id = %request_id,
            operation = operation_name,
            target = operation_target.as_deref(),
        ),
        Level::ERROR => tracing::error_span!(
            "mcp_request",
            method = %method,
            request_id = %request_id,
            operation = operation_name,
            target = operation_target.as_deref(),
        ),
    }
}

/// Log successful request completion at the configured level.
fn log_success(level: Level, method: &str, duration_ms: f64) {
    match level {
        Level::TRACE => {
            tracing::trace!(method = %method, duration_ms = duration_ms, "MCP request completed")
        }
        Level::DEBUG => {
            tracing::debug!(method = %method, duration_ms = duration_ms, "MCP request completed")
        }
        Level::INFO => {
            tracing::info!(method = %method, duration_ms = duration_ms, "MCP request completed")
        }
        Level::WARN => {
            tracing::warn!(method = %method, duration_ms = duration_ms, "MCP request completed")
        }
        Level::ERROR => {
            tracing::error!(method = %method, duration_ms = duration_ms, "MCP request completed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_layer_creation() {
        let layer = McpTracingLayer::new();
        assert_eq!(layer.level, Level::INFO);

        let layer = McpTracingLayer::new().level(Level::DEBUG);
        assert_eq!(layer.level, Level::DEBUG);
    }

    #[test]
    fn test_extract_operation_details() {
        use crate::protocol::{CallToolParams, GetPromptParams, ReadResourceParams};
        use serde_json::Value;
        use std::collections::HashMap;

        // Test tool call
        let req = McpRequest::CallTool(CallToolParams {
            name: "my_tool".to_string(),
            arguments: Value::Null,
            meta: None,
            task: None,
        });
        let (name, target) = extract_operation_details(&req);
        assert_eq!(name, Some("tool"));
        assert_eq!(target, Some("my_tool".to_string()));

        // Test resource read
        let req = McpRequest::ReadResource(ReadResourceParams {
            uri: "file:///test.txt".to_string(),
        });
        let (name, target) = extract_operation_details(&req);
        assert_eq!(name, Some("resource"));
        assert_eq!(target, Some("file:///test.txt".to_string()));

        // Test prompt get
        let req = McpRequest::GetPrompt(GetPromptParams {
            name: "my_prompt".to_string(),
            arguments: HashMap::new(),
        });
        let (name, target) = extract_operation_details(&req);
        assert_eq!(name, Some("prompt"));
        assert_eq!(target, Some("my_prompt".to_string()));

        // Test ping
        let req = McpRequest::Ping;
        let (name, target) = extract_operation_details(&req);
        assert_eq!(name, Some("ping"));
        assert_eq!(target, None);
    }
}
