//! Stdio transport for MCP
//!
//! Reads JSON-RPC messages from stdin and writes responses to stdout.
//! Uses line-delimited JSON format.

use std::io::{self, BufRead, Write};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::error::{Error, Result};
use crate::protocol::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcResponse, JsonRpcResponseMessage,
};
use crate::router::{JsonRpcService, McpRouter};

/// Stdio transport for MCP servers
///
/// Reads JSON-RPC messages from stdin and writes responses to stdout.
/// Supports both single requests and batch requests.
///
/// # Example
///
/// ```rust,no_run
/// use tower_mcp::{McpRouter, StdioTransport};
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let router = McpRouter::new()
///         .server_info("my-server", "1.0.0");
///
///     let mut transport = StdioTransport::new(router);
///     transport.run().await?;
///     Ok(())
/// }
/// ```
pub struct StdioTransport {
    service: JsonRpcService<McpRouter>,
    router: McpRouter,
}

impl StdioTransport {
    /// Create a new stdio transport wrapping an MCP router
    pub fn new(router: McpRouter) -> Self {
        let service = JsonRpcService::new(router.clone());
        Self { service, router }
    }

    /// Run the transport, processing messages until EOF or error
    pub async fn run(&mut self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        tracing::info!("Stdio transport started, waiting for input");

        loop {
            line.clear();
            let bytes_read = reader
                .read_line(&mut line)
                .await
                .map_err(|e| Error::Transport(format!("Failed to read from stdin: {}", e)))?;

            if bytes_read == 0 {
                // EOF
                tracing::info!("Stdin closed, shutting down");
                break;
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            tracing::debug!(input = %trimmed, "Received message");

            // Try to parse as a message (single or batch)
            match self.process_line(trimmed).await {
                Ok(Some(response)) => {
                    let response_json = serde_json::to_string(&response).map_err(|e| {
                        Error::Transport(format!("Failed to serialize response: {}", e))
                    })?;
                    tracing::debug!(output = %response_json, "Sending response");
                    stdout
                        .write_all(response_json.as_bytes())
                        .await
                        .map_err(|e| {
                            Error::Transport(format!("Failed to write to stdout: {}", e))
                        })?;
                    stdout
                        .write_all(b"\n")
                        .await
                        .map_err(|e| Error::Transport(format!("Failed to write newline: {}", e)))?;
                    stdout
                        .flush()
                        .await
                        .map_err(|e| Error::Transport(format!("Failed to flush stdout: {}", e)))?;
                }
                Ok(None) => {
                    // Notification, no response needed
                }
                Err(e) => {
                    tracing::error!(error = %e, "Error processing message");
                    // Send error response
                    let error_response = JsonRpcResponse::error(
                        None,
                        crate::error::JsonRpcError::parse_error(e.to_string()),
                    );
                    let response_json = serde_json::to_string(&error_response).map_err(|e| {
                        Error::Transport(format!("Failed to serialize error: {}", e))
                    })?;
                    stdout
                        .write_all(response_json.as_bytes())
                        .await
                        .map_err(|e| Error::Transport(format!("Failed to write error: {}", e)))?;
                    stdout
                        .write_all(b"\n")
                        .await
                        .map_err(|e| Error::Transport(format!("Failed to write newline: {}", e)))?;
                    stdout
                        .flush()
                        .await
                        .map_err(|e| Error::Transport(format!("Failed to flush stdout: {}", e)))?;
                }
            }
        }

        Ok(())
    }

    /// Process a single line of input
    async fn process_line(&mut self, line: &str) -> Result<Option<JsonRpcResponseMessage>> {
        // Check if it's a notification (no id field)
        let parsed: serde_json::Value = serde_json::from_str(line)?;
        if parsed.get("id").is_none()
            && let Ok(notification) = serde_json::from_str::<JsonRpcNotification>(line)
        {
            self.handle_notification(notification)?;
            return Ok(None);
        }

        // Try to parse as a message (single request or batch)
        let message: JsonRpcMessage = serde_json::from_str(line)?;
        let response = self.service.call_message(message).await?;
        Ok(Some(response))
    }

    /// Handle a notification
    fn handle_notification(&mut self, notification: JsonRpcNotification) -> Result<()> {
        let mcp_notification = crate::protocol::McpNotification::from_jsonrpc(&notification)?;
        self.router.handle_notification(mcp_notification);
        Ok(())
    }
}

/// Synchronous stdio transport for simpler use cases
///
/// This version uses blocking I/O and is suitable for simple CLI tools.
pub struct SyncStdioTransport {
    service: JsonRpcService<McpRouter>,
    router: McpRouter,
}

impl SyncStdioTransport {
    /// Create a new synchronous stdio transport
    pub fn new(router: McpRouter) -> Self {
        let service = JsonRpcService::new(router.clone());
        Self { service, router }
    }

    /// Run the transport synchronously using a tokio runtime
    pub fn run_blocking(&mut self) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| Error::Transport(format!("Failed to create runtime: {}", e)))?;

        let stdin = io::stdin();
        let mut stdout = io::stdout();

        tracing::info!("Sync stdio transport started");

        for line in stdin.lock().lines() {
            let line =
                line.map_err(|e| Error::Transport(format!("Failed to read from stdin: {}", e)))?;

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            tracing::debug!(input = %trimmed, "Received message");

            match rt.block_on(self.process_line(trimmed)) {
                Ok(Some(response)) => {
                    let response_json = serde_json::to_string(&response).map_err(|e| {
                        Error::Transport(format!("Failed to serialize response: {}", e))
                    })?;
                    tracing::debug!(output = %response_json, "Sending response");
                    writeln!(stdout, "{}", response_json).map_err(|e| {
                        Error::Transport(format!("Failed to write to stdout: {}", e))
                    })?;
                    stdout
                        .flush()
                        .map_err(|e| Error::Transport(format!("Failed to flush stdout: {}", e)))?;
                }
                Ok(None) => {
                    // Notification, no response
                }
                Err(e) => {
                    tracing::error!(error = %e, "Error processing message");
                    let error_response = JsonRpcResponse::error(
                        None,
                        crate::error::JsonRpcError::parse_error(e.to_string()),
                    );
                    let response_json = serde_json::to_string(&error_response).map_err(|e| {
                        Error::Transport(format!("Failed to serialize error: {}", e))
                    })?;
                    writeln!(stdout, "{}", response_json)
                        .map_err(|e| Error::Transport(format!("Failed to write error: {}", e)))?;
                    stdout
                        .flush()
                        .map_err(|e| Error::Transport(format!("Failed to flush stdout: {}", e)))?;
                }
            }
        }

        tracing::info!("Stdin closed, shutting down");
        Ok(())
    }

    async fn process_line(&mut self, line: &str) -> Result<Option<JsonRpcResponseMessage>> {
        // Check for notification (no id field)
        let parsed: serde_json::Value = serde_json::from_str(line)?;
        if parsed.get("id").is_none()
            && let Ok(notification) = serde_json::from_str::<JsonRpcNotification>(line)
        {
            self.handle_notification(notification)?;
            return Ok(None);
        }

        let message: JsonRpcMessage = serde_json::from_str(line)?;
        let response = self.service.call_message(message).await?;
        Ok(Some(response))
    }

    fn handle_notification(&mut self, notification: JsonRpcNotification) -> Result<()> {
        let mcp_notification = crate::protocol::McpNotification::from_jsonrpc(&notification)?;
        self.router.handle_notification(mcp_notification);
        Ok(())
    }
}
