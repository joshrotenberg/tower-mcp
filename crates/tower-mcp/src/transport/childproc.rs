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
//!             "protocolVersion": "2025-11-25",
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

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};

use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

use crate::error::{Error, Result};
use crate::framing::{FrameReader, InputFrame, clean_input_line};
use crate::protocol::{JsonRpcRequest, JsonRpcResponse, RequestId};

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
///
/// # Response correlation and notifications
///
/// `send_request` reads frames from the child's stdout until it finds the
/// response whose id matches the request it just sent, rather than assuming
/// the next line is the answer. A spec-compliant child may write
/// notifications (progress, log messages, list-changed) before it answers a
/// request; those are logged and dropped, since this connection has no
/// channel to publish them on -- there is no background reader task or
/// subscriber here, unlike [`crate::client::McpClient`]. A response whose id
/// does not match the one currently being awaited is not dropped: it is set
/// aside in a small pending map, so a later call waiting on that id can pick
/// it up immediately instead of losing a response that arrived out of order
/// (#1334).
pub struct ChildProcessConnection {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: FrameReader<tokio::process::ChildStdout>,
    request_id: AtomicI64,
    /// Responses read ahead of the request they answer, keyed by id, waiting
    /// for the `send_request` call that is watching for that id.
    pending: HashMap<RequestId, JsonRpcResponse>,
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
            stdout: FrameReader::new(stdout),
            request_id: AtomicI64::new(1),
            pending: HashMap::new(),
        })
    }

    /// Send a JSON-RPC request and wait for response
    pub async fn send_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = RequestId::Number(self.request_id.fetch_add(1, Ordering::Relaxed));
        let request = JsonRpcRequest::new(id.clone(), method).with_params(params);

        // Send request
        let request_json = serde_json::to_string(&request)
            .map_err(|e| Error::Transport(format!("Failed to serialize request: {}", e)))?;

        tracing::debug!(method = %method, id = ?id, "Sending request to child");

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

        self.response_for(&id).await
    }

    /// Read frames until the response for `id` arrives.
    ///
    /// Notifications are logged and discarded. A response for some other id
    /// is held in `pending` rather than discarded, since it answers a request
    /// this connection already sent and a future call will be watching for
    /// it. A frame that is not valid UTF-8, per [`FrameReader`], costs only
    /// itself; the loop reads on.
    async fn response_for(&mut self, id: &RequestId) -> Result<serde_json::Value> {
        if let Some(response) = self.pending.remove(id) {
            return Self::into_result(response);
        }

        loop {
            let frame = self
                .stdout
                .next_frame()
                .await?
                .ok_or_else(|| Error::Transport("Child process closed stdout".to_string()))?;

            let line = match frame {
                InputFrame::Line(line) => line,
                InputFrame::Undecodable => {
                    tracing::warn!(
                        "invalid UTF-8 in a frame from the child, discarding it; \
                         a response carried in it will not arrive"
                    );
                    continue;
                }
            };

            let line = clean_input_line(&line);
            if line.is_empty() {
                continue;
            }

            tracing::debug!(response = %line, "Received frame from child");

            let value: serde_json::Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to parse frame from child, discarding it");
                    continue;
                }
            };

            // A response carries "result" or "error"; a notification (or a
            // request, which this connection does not expect from a child)
            // carries neither and is dropped.
            if value.get("result").is_none() && value.get("error").is_none() {
                tracing::debug!(
                    method = ?value.get("method").and_then(|m| m.as_str()),
                    "dropping a notification from the child"
                );
                continue;
            }

            let response: JsonRpcResponse = match serde_json::from_value(value) {
                Ok(response) => response,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to parse response from child, discarding it");
                    continue;
                }
            };

            let response_id = match &response {
                JsonRpcResponse::Result(r) => Some(r.id.clone()),
                JsonRpcResponse::Error(e) => e.id.clone(),
                _ => None,
            };

            match response_id {
                Some(response_id) if &response_id == id => {
                    return Self::into_result(response);
                }
                Some(response_id) => {
                    self.pending.insert(response_id, response);
                }
                None => {
                    tracing::warn!("child sent a response with no id, discarding it");
                }
            }
        }
    }

    fn into_result(response: JsonRpcResponse) -> Result<serde_json::Value> {
        match response {
            JsonRpcResponse::Result(r) => Ok(r.result),
            JsonRpcResponse::Error(e) => Err(Error::JsonRpc(e.error)),
            _ => Err(Error::Transport("unexpected response variant".to_string())),
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
                "protocolVersion": "2025-11-25",
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

    #[tokio::test]
    async fn test_transport_env() {
        let transport = ChildProcessTransport::new("prog")
            .env("KEY1", "val1")
            .env("KEY2", "val2");

        assert_eq!(transport.envs.len(), 2);
        assert_eq!(transport.envs[0], ("KEY1".to_string(), "val1".to_string()));
    }

    #[tokio::test]
    async fn test_spawn_nonexistent_fails() {
        let result = ChildProcessTransport::new("nonexistent-program-xyz-123")
            .spawn()
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_spawn_and_communicate() {
        // A minimal scripted server: read the request line, ignore it, and
        // answer with a response matching the id `send_request` sent. `cat`
        // used to stand in here, but its echoed request has no
        // "result"/"error" field, so under the fix (#1334) it reads as a
        // notification and is correctly dropped rather than surfaced as an
        // error -- there is nothing left to assert without a real answer.
        let mut conn = ChildProcessTransport::new("sh")
            .arg("-c")
            .arg(r#"read -r _line; printf '{"jsonrpc":"2.0","id":1,"result":{"echoed":true}}\n'"#)
            .spawn()
            .await
            .unwrap();

        assert!(conn.is_running());

        let response = conn
            .send_request("echo", serde_json::json!({"msg": "hello"}))
            .await
            .unwrap();

        assert_eq!(response, serde_json::json!({"echoed": true}));
    }

    #[tokio::test]
    async fn test_shutdown_graceful() {
        let conn = ChildProcessTransport::new("cat").spawn().await.unwrap();
        // Shutdown should succeed - cat exits when stdin is closed
        conn.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_is_running_after_exit() {
        // `true` exits immediately, but spawn plus exit plus reap is not
        // instantaneous, especially on loaded Windows CI runners where a
        // fixed 100 ms sleep flaked. Poll with a generous bound instead.
        let mut conn = ChildProcessTransport::new("true").spawn().await.unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while conn.is_running() {
            assert!(
                std::time::Instant::now() < deadline,
                "child process still reported running 10s after exit"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn test_send_notification() {
        let mut conn = ChildProcessTransport::new("cat").spawn().await.unwrap();
        // Notification should succeed (no response expected)
        conn.send_notification("test/notify", serde_json::json!({"data": 1}))
            .await
            .unwrap();
        conn.shutdown().await.unwrap();
    }
}
