//! MCP Client implementation
//!
//! Provides client functionality for connecting to MCP servers.
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::client::{McpClient, StdioClientTransport};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Connect to an MCP server via stdio
//!     let transport = StdioClientTransport::spawn("my-mcp-server", &["--flag"]).await?;
//!     let mut client = McpClient::new(transport);
//!
//!     // Initialize the connection
//!     let server_info = client.initialize("my-client", "1.0.0").await?;
//!     println!("Connected to: {}", server_info.server_info.name);
//!
//!     // List available tools
//!     let tools = client.list_tools().await?;
//!     for tool in &tools.tools {
//!         println!("Tool: {}", tool.name);
//!     }
//!
//!     // Call a tool
//!     let result = client.call_tool("my-tool", serde_json::json!({"arg": "value"})).await?;
//!     println!("Result: {:?}", result);
//!
//!     Ok(())
//! }
//! ```

use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::error::{Error, Result};
use crate::protocol::{
    CallToolParams, CallToolResult, ClientCapabilities, GetPromptParams, GetPromptResult,
    Implementation, InitializeParams, InitializeResult, JsonRpcRequest, JsonRpcResponse,
    ListPromptsParams, ListPromptsResult, ListResourcesParams, ListResourcesResult,
    ListToolsParams, ListToolsResult, ReadResourceParams, ReadResourceResult,
};

/// Trait for MCP client transports
#[async_trait]
pub trait ClientTransport: Send {
    /// Send a request and receive a response
    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value>;

    /// Send a notification (no response expected)
    async fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<()>;

    /// Check if the transport is still connected
    fn is_connected(&self) -> bool;

    /// Close the transport
    async fn close(self: Box<Self>) -> Result<()>;
}

/// MCP Client for connecting to MCP servers
pub struct McpClient<T: ClientTransport> {
    transport: T,
    initialized: bool,
    server_info: Option<InitializeResult>,
}

impl<T: ClientTransport> McpClient<T> {
    /// Create a new MCP client with the given transport
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            initialized: false,
            server_info: None,
        }
    }

    /// Get the server info (available after initialization)
    pub fn server_info(&self) -> Option<&InitializeResult> {
        self.server_info.as_ref()
    }

    /// Check if the client is initialized
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Initialize the MCP connection
    pub async fn initialize(
        &mut self,
        client_name: &str,
        client_version: &str,
    ) -> Result<&InitializeResult> {
        let params = InitializeParams {
            protocol_version: crate::protocol::LATEST_PROTOCOL_VERSION.to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: Implementation {
                name: client_name.to_string(),
                version: client_version.to_string(),
            },
        };

        let result: InitializeResult = self.request("initialize", &params).await?;
        self.server_info = Some(result);

        // Send initialized notification
        self.transport
            .notify("notifications/initialized", serde_json::json!({}))
            .await?;

        self.initialized = true;

        Ok(self.server_info.as_ref().unwrap())
    }

    /// List available tools
    pub async fn list_tools(&mut self) -> Result<ListToolsResult> {
        self.ensure_initialized()?;
        self.request("tools/list", &ListToolsParams { cursor: None })
            .await
    }

    /// Call a tool
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult> {
        self.ensure_initialized()?;
        let params = CallToolParams {
            name: name.to_string(),
            arguments,
            meta: None,
        };
        self.request("tools/call", &params).await
    }

    /// List available resources
    pub async fn list_resources(&mut self) -> Result<ListResourcesResult> {
        self.ensure_initialized()?;
        self.request("resources/list", &ListResourcesParams { cursor: None })
            .await
    }

    /// Read a resource
    pub async fn read_resource(&mut self, uri: &str) -> Result<ReadResourceResult> {
        self.ensure_initialized()?;
        let params = ReadResourceParams {
            uri: uri.to_string(),
        };
        self.request("resources/read", &params).await
    }

    /// List available prompts
    pub async fn list_prompts(&mut self) -> Result<ListPromptsResult> {
        self.ensure_initialized()?;
        self.request("prompts/list", &ListPromptsParams { cursor: None })
            .await
    }

    /// Get a prompt
    pub async fn get_prompt(
        &mut self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<GetPromptResult> {
        self.ensure_initialized()?;
        let params = GetPromptParams {
            name: name.to_string(),
            arguments: arguments.unwrap_or_default(),
        };
        self.request("prompts/get", &params).await
    }

    /// Ping the server
    pub async fn ping(&mut self) -> Result<()> {
        let _: serde_json::Value = self.request("ping", &serde_json::json!({})).await?;
        Ok(())
    }

    /// Send a raw request
    pub async fn request<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: &P,
    ) -> Result<R> {
        let params_value = serde_json::to_value(params)
            .map_err(|e| Error::Transport(format!("Failed to serialize params: {}", e)))?;

        let result = self.transport.request(method, params_value).await?;

        serde_json::from_value(result)
            .map_err(|e| Error::Transport(format!("Failed to deserialize response: {}", e)))
    }

    /// Send a notification
    pub async fn notify<P: serde::Serialize>(&mut self, method: &str, params: &P) -> Result<()> {
        let params_value = serde_json::to_value(params)
            .map_err(|e| Error::Transport(format!("Failed to serialize params: {}", e)))?;

        self.transport.notify(method, params_value).await
    }

    fn ensure_initialized(&self) -> Result<()> {
        if !self.initialized {
            return Err(Error::Transport("Client not initialized".to_string()));
        }
        Ok(())
    }
}

// ============================================================================
// Stdio Client Transport
// ============================================================================

/// Client transport that communicates with a subprocess via stdio
pub struct StdioClientTransport {
    child: Option<Child>,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    request_id: AtomicI64,
}

impl StdioClientTransport {
    /// Spawn a new subprocess and connect to it
    pub async fn spawn(program: &str, args: &[&str]) -> Result<Self> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Transport(format!("Failed to spawn {}: {}", program, e)))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Transport("Failed to get child stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Transport("Failed to get child stdout".to_string()))?;

        tracing::info!(program = %program, "Spawned MCP server process");

        Ok(Self {
            child: Some(child),
            stdin,
            stdout: BufReader::new(stdout),
            request_id: AtomicI64::new(1),
        })
    }

    /// Create from an existing child process
    pub fn from_child(mut child: Child) -> Result<Self> {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Transport("Failed to get child stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Transport("Failed to get child stdout".to_string()))?;

        Ok(Self {
            child: Some(child),
            stdin,
            stdout: BufReader::new(stdout),
            request_id: AtomicI64::new(1),
        })
    }

    async fn send_line(&mut self, line: &str) -> Result<()> {
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| Error::Transport(format!("Failed to write: {}", e)))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| Error::Transport(format!("Failed to write newline: {}", e)))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| Error::Transport(format!("Failed to flush: {}", e)))?;
        Ok(())
    }

    async fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .await
            .map_err(|e| Error::Transport(format!("Failed to read: {}", e)))?;

        if line.is_empty() {
            return Err(Error::Transport("Connection closed".to_string()));
        }

        Ok(line)
    }
}

#[async_trait]
impl ClientTransport for StdioClientTransport {
    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.request_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest::new(id, method).with_params(params);

        let request_json = serde_json::to_string(&request)
            .map_err(|e| Error::Transport(format!("Failed to serialize: {}", e)))?;

        tracing::debug!(method = %method, id = %id, "Sending request");
        self.send_line(&request_json).await?;

        let response_line = self.read_line().await?;
        tracing::debug!(response = %response_line.trim(), "Received response");

        let response: JsonRpcResponse = serde_json::from_str(response_line.trim())
            .map_err(|e| Error::Transport(format!("Failed to parse response: {}", e)))?;

        match response {
            JsonRpcResponse::Result(r) => Ok(r.result),
            JsonRpcResponse::Error(e) => Err(Error::JsonRpc(e.error)),
        }
    }

    async fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<()> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        let json = serde_json::to_string(&notification)
            .map_err(|e| Error::Transport(format!("Failed to serialize: {}", e)))?;

        tracing::debug!(method = %method, "Sending notification");
        self.send_line(&json).await
    }

    fn is_connected(&self) -> bool {
        // Assume connected if we have a child process handle
        self.child.is_some()
    }

    async fn close(mut self: Box<Self>) -> Result<()> {
        // Close stdin to signal EOF
        drop(self.stdin);

        if let Some(mut child) = self.child.take() {
            // Wait for process with timeout
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;

            match result {
                Ok(Ok(status)) => {
                    tracing::info!(status = ?status, "Child process exited");
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "Error waiting for child");
                }
                Err(_) => {
                    tracing::warn!("Timeout waiting for child, killing");
                    let _ = child.kill().await;
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_client_not_initialized() {
        // Create a mock transport for testing
        struct MockTransport;

        #[async_trait]
        impl ClientTransport for MockTransport {
            async fn request(
                &mut self,
                _: &str,
                _: serde_json::Value,
            ) -> Result<serde_json::Value> {
                Ok(serde_json::json!({}))
            }
            async fn notify(&mut self, _: &str, _: serde_json::Value) -> Result<()> {
                Ok(())
            }
            fn is_connected(&self) -> bool {
                true
            }
            async fn close(self: Box<Self>) -> Result<()> {
                Ok(())
            }
        }

        let mut client = McpClient::new(MockTransport);

        // Should fail because not initialized
        let result = client.list_tools().await;
        assert!(result.is_err());
    }
}
