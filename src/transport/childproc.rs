//! Child process transport for MCP
//!
//! Spawns and communicates with subprocess MCP servers via stdio.
//! Useful for:
//! - Running untrusted MCP servers in isolation
//! - Spawning tool-specific servers on demand
//! - Testing
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::BoxError;
//! use tower_mcp::transport::childproc::ChildProcessTransport;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), BoxError> {
//!     // Spawn an MCP server as a child process
//!     let mut transport = ChildProcessTransport::new("my-mcp-server")
//!         .arg("--some-flag")
//!         .spawn()
//!         .await?;
//!
//!     // Send a request
//!     let response = transport.send_request(
//!         "initialize",
//!         serde_json::json!({
//!             "protocolVersion": "2025-03-26",
//!             "capabilities": {},
//!             "clientInfo": { "name": "my-client", "version": "1.0" }
//!         })
//!     ).await?;
//!
//!     // Shutdown
//!     transport.shutdown().await?;
//!     Ok(())
//! }
//! ```

use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::error::{Error, Result};
use crate::protocol::{JsonRpcRequest, JsonRpcResponse};

/// Builder for child process transport
pub struct ChildProcessTransport {
    program: String,
    args: Vec<String>,
    envs: Vec<(String, String)>,
}

impl ChildProcessTransport {
    /// Create a new child process transport builder
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            envs: Vec::new(),
        }
    }

    /// Add a command-line argument
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Add multiple command-line arguments
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(|s| s.into()));
        self
    }

    /// Set an environment variable
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    /// Spawn the child process
    pub async fn spawn(self) -> Result<ChildProcessConnection> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        for (key, value) in &self.envs {
            cmd.env(key, value);
        }

        let child = cmd
            .spawn()
            .map_err(|e| Error::Transport(format!("Failed to spawn {}: {}", self.program, e)))?;

        tracing::info!(program = %self.program, "Spawned child process");

        ChildProcessConnection::new(child)
    }
}

/// Active connection to a child MCP server process
pub struct ChildProcessConnection {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    request_id: AtomicI64,
}

impl ChildProcessConnection {
    fn new(mut child: Child) -> Result<Self> {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Transport("Failed to get child stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Transport("Failed to get child stdout".to_string()))?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            request_id: AtomicI64::new(1),
        })
    }

    /// Send a JSON-RPC request and wait for response
    pub async fn send_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.request_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest::new(id, method).with_params(params);

        // Send request
        let request_json = serde_json::to_string(&request)
            .map_err(|e| Error::Transport(format!("Failed to serialize request: {}", e)))?;

        tracing::debug!(method = %method, id = %id, "Sending request to child");

        self.stdin
            .write_all(request_json.as_bytes())
            .await
            .map_err(|e| Error::Transport(format!("Failed to write to child stdin: {}", e)))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| Error::Transport(format!("Failed to write newline: {}", e)))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| Error::Transport(format!("Failed to flush stdin: {}", e)))?;

        // Read response
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .await
            .map_err(|e| Error::Transport(format!("Failed to read from child stdout: {}", e)))?;

        if line.is_empty() {
            return Err(Error::Transport("Child process closed stdout".to_string()));
        }

        tracing::debug!(response = %line.trim(), "Received response from child");

        let response: JsonRpcResponse = serde_json::from_str(line.trim())
            .map_err(|e| Error::Transport(format!("Failed to parse response: {}", e)))?;

        match response {
            JsonRpcResponse::Result(r) => Ok(r.result),
            JsonRpcResponse::Error(e) => Err(Error::JsonRpc(e.error)),
        }
    }

    /// Send a notification (no response expected)
    pub async fn send_notification(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<()> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        let json = serde_json::to_string(&notification)
            .map_err(|e| Error::Transport(format!("Failed to serialize notification: {}", e)))?;

        tracing::debug!(method = %method, "Sending notification to child");

        self.stdin
            .write_all(json.as_bytes())
            .await
            .map_err(|e| Error::Transport(format!("Failed to write notification: {}", e)))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| Error::Transport(format!("Failed to write newline: {}", e)))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| Error::Transport(format!("Failed to flush stdin: {}", e)))?;

        Ok(())
    }

    /// Initialize the MCP connection
    pub async fn initialize(
        &mut self,
        client_name: &str,
        client_version: &str,
    ) -> Result<serde_json::Value> {
        self.send_request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {
                    "name": client_name,
                    "version": client_version
                }
            }),
        )
        .await
    }

    /// Send initialized notification
    pub async fn send_initialized(&mut self) -> Result<()> {
        self.send_notification("notifications/initialized", serde_json::json!({}))
            .await
    }

    /// List available tools
    pub async fn list_tools(&mut self) -> Result<serde_json::Value> {
        self.send_request("tools/list", serde_json::json!({})).await
    }

    /// Call a tool
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.send_request(
            "tools/call",
            serde_json::json!({
                "name": name,
                "arguments": arguments
            }),
        )
        .await
    }

    /// Check if the child process is still running
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Gracefully shutdown the child process
    pub async fn shutdown(mut self) -> Result<()> {
        // Close stdin to signal EOF
        drop(self.stdin);

        // Wait for process to exit with timeout
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait()).await;

        match result {
            Ok(Ok(status)) => {
                tracing::info!(status = ?status, "Child process exited");
                Ok(())
            }
            Ok(Err(e)) => {
                tracing::error!(error = %e, "Error waiting for child process");
                Err(Error::Transport(format!("Child process error: {}", e)))
            }
            Err(_) => {
                // Timeout - kill the process
                tracing::warn!("Child process did not exit gracefully, killing");
                self.child
                    .kill()
                    .await
                    .map_err(|e| Error::Transport(format!("Failed to kill child: {}", e)))?;
                Ok(())
            }
        }
    }

    /// Kill the child process immediately
    pub async fn kill(mut self) -> Result<()> {
        self.child
            .kill()
            .await
            .map_err(|e| Error::Transport(format!("Failed to kill child: {}", e)))?;
        tracing::info!("Child process killed");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_transport_builder() {
        let transport = ChildProcessTransport::new("echo")
            .arg("hello")
            .env("FOO", "bar");

        assert_eq!(transport.program, "echo");
        assert_eq!(transport.args, vec!["hello"]);
        assert_eq!(transport.envs, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[tokio::test]
    async fn test_transport_args() {
        let transport = ChildProcessTransport::new("cmd").args(["--flag1", "--flag2"]);

        assert_eq!(transport.args, vec!["--flag1", "--flag2"]);
    }
}
