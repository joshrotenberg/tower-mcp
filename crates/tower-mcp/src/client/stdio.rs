//! Stdio client transport for subprocess MCP servers.
//!
//! Provides [`StdioClientTransport`] which spawns a child process and
//! communicates using line-delimited JSON over stdin/stdout.
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::client::{McpClient, StdioClientTransport};
//!
//! # async fn example() -> Result<(), tower_mcp::BoxError> {
//! let transport = StdioClientTransport::spawn("my-mcp-server", &["--flag"]).await?;
//! let client = McpClient::connect(transport).await?;
//! # Ok(())
//! # }
//! ```

use std::ffi::OsStr;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use super::transport::ClientTransport;
use crate::error::{Error, Result};

/// Client transport that communicates with a subprocess via stdio.
///
/// Spawns a child process and communicates using line-delimited JSON-RPC
/// messages over stdin (write) and stdout (read). Stderr is inherited so
/// server debug output appears in the client's terminal.
pub struct StdioClientTransport {
    child: Option<Child>,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: BufReader<tokio::process::ChildStdout>,
}

impl StdioClientTransport {
    /// Spawn a new subprocess and connect to it.
    ///
    /// # Errors
    ///
    /// Returns an error if the process fails to spawn or if stdin/stdout
    /// handles cannot be acquired.
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
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
        })
    }

    /// Spawn a subprocess with full control over the [`Command`].
    ///
    /// Use this when you need to set environment variables, working directory,
    /// or other process attributes beyond what [`spawn()`](Self::spawn) offers.
    ///
    /// The provided `Command` is configured with piped stdin/stdout and
    /// inherited stderr before spawning.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tokio::process::Command;
    /// use tower_mcp::client::StdioClientTransport;
    ///
    /// # async fn example() -> Result<(), tower_mcp::BoxError> {
    /// let mut cmd = Command::new("npx");
    /// cmd.args(["-y", "@modelcontextprotocol/server-github"]);
    /// cmd.env("GITHUB_TOKEN", "ghp_...");
    /// cmd.current_dir("/tmp");
    ///
    /// let transport = StdioClientTransport::spawn_command(cmd).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn spawn_command(mut cmd: Command) -> Result<Self> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let program = format!("{:?}", cmd.as_std().get_program());

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
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
        })
    }

    /// Spawn a subprocess with environment variables.
    ///
    /// Convenience method for the common case of spawning a server
    /// with specific environment variables (e.g., API tokens).
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::client::StdioClientTransport;
    ///
    /// # async fn example() -> Result<(), tower_mcp::BoxError> {
    /// let transport = StdioClientTransport::spawn_with_env(
    ///     "npx",
    ///     &["-y", "@modelcontextprotocol/server-github"],
    ///     &[("GITHUB_TOKEN", "ghp_...")],
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn spawn_with_env<K, V>(
        program: &str,
        args: &[&str],
        env: &[(K, V)],
    ) -> Result<Self>
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.envs(env.iter().map(|(k, v)| (k.as_ref(), v.as_ref())));
        Self::spawn_command(cmd).await
    }

    /// Create from an existing child process.
    ///
    /// The child must have piped stdin and stdout.
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
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
        })
    }
}

#[async_trait]
impl ClientTransport for StdioClientTransport {
    async fn send(&mut self, message: &str) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| Error::Transport("Transport closed".to_string()))?;

        stdin
            .write_all(message.as_bytes())
            .await
            .map_err(|e| Error::Transport(format!("Failed to write: {}", e)))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| Error::Transport(format!("Failed to write newline: {}", e)))?;
        stdin
            .flush()
            .await
            .map_err(|e| Error::Transport(format!("Failed to flush: {}", e)))?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<String>> {
        let mut line = String::new();
        let bytes = self
            .stdout
            .read_line(&mut line)
            .await
            .map_err(|e| Error::Transport(format!("Failed to read: {}", e)))?;

        if bytes == 0 {
            return Ok(None); // EOF
        }

        Ok(Some(line.trim().to_string()))
    }

    fn is_connected(&self) -> bool {
        self.child.is_some() && self.stdin.is_some()
    }

    async fn close(&mut self) -> Result<()> {
        // Drop stdin to signal EOF to the child process
        self.stdin.take();

        if let Some(mut child) = self.child.take() {
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
