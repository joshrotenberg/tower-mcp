//! Stdio client transport for subprocess MCP servers.
//!
//! Provides [`StdioClientTransport`] which spawns a child process and
//! communicates using line-delimited JSON over stdin/stdout.
//!
//! # Malformed output from the server
//!
//! Frames are delimited by newline bytes and decoded one at a time, so a
//! frame the server writes malformed costs that frame and nothing else. A
//! frame that is not valid UTF-8, a stray debug print or a mis-encoded log
//! line on stdout, is logged and discarded, and the transport reads on.
//!
//! Discarding is what a client has available. The server side answers the
//! same input with a JSON-RPC parse error and keeps serving (#1271); a client
//! has nobody to send that to. The alternative, reporting it as a transport
//! error, ends the connection and fails every pending request, which lets a
//! server that is otherwise working correctly disconnect its client with one
//! stray byte (#1296). It is also the treatment the layer above already gives
//! JSON that does not parse: warn, drop the message, keep the connection.
//!
//! One consequence is worth stating plainly. If the discarded frame held the
//! response to a pending request, that request is not failed early; it waits
//! for its own timeout, exactly as it would have if the server had never
//! answered. Failing it early would need the request id, which is inside the
//! bytes that would not decode.
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

use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

use super::transport::ClientTransport;
use crate::error::{Error, Result};
use crate::framing::{FrameReader, InputFrame, clean_input_line};

/// Client transport that communicates with a subprocess via stdio.
///
/// Spawns a child process and communicates using line-delimited JSON-RPC
/// messages over stdin (write) and stdout (read). By default stderr is
/// inherited so server debug output appears in the client's terminal. A
/// caller using [`Self::spawn_command`] may redirect or pipe it instead.
pub struct StdioClientTransport {
    child: Option<Child>,
    stdin: Option<tokio::process::ChildStdin>,
    // `FrameReader` retains a partially read frame when its future is
    // cancelled by the client's `select!` loop. A bare `read_until` future can
    // discard those bytes while leaving the newline behind, turning a valid
    // response into an empty frame when outgoing commands arrive concurrently.
    // It also frames over bytes rather than decoded text, so output the
    // decoder rejects costs one frame instead of the connection (#1296).
    stdout: FrameReader<tokio::process::ChildStdout>,
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
        cmd.args(args);
        Self::spawn_command(&mut cmd).await
    }

    /// Spawn from a pre-configured [`Command`].
    ///
    /// This allows setting environment variables, working directory, and
    /// other process configuration before spawning.
    ///
    /// Stdin and stdout are automatically set to piped. Stderr keeps the
    /// [`Command`] configuration; its default is inherited.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tokio::process::Command;
    /// use tower_mcp::client::StdioClientTransport;
    ///
    /// # async fn example() -> Result<(), tower_mcp::BoxError> {
    /// let mut cmd = Command::new("npx");
    /// cmd.args(["-y", "@modelcontextprotocol/server-github"])
    ///    .env("GITHUB_TOKEN", "ghp_...");
    /// let transport = StdioClientTransport::spawn_command(&mut cmd).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn spawn_command(cmd: &mut Command) -> Result<Self> {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Transport(format!("Failed to spawn process: {}", e)))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Transport("Failed to get child stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Transport("Failed to get child stdout".to_string()))?;

        tracing::info!("Spawned MCP server process");

        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            stdout: FrameReader::new(stdout),
        })
    }

    /// Take the child's piped stderr handle, if the command configured one.
    ///
    /// This returns `None` when stderr is inherited, redirected elsewhere, or
    /// has already been taken. It is useful for clients that need to integrate
    /// server diagnostics with their own terminal or logging UI.
    pub fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child.as_mut()?.stderr.take()
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
            stdout: FrameReader::new(stdout),
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

    /// Read the next message the server wrote.
    ///
    /// Frames that are not valid UTF-8 are logged and skipped rather than
    /// reported, so `Err` keeps meaning the connection is over and `Ok(None)`
    /// keeps meaning EOF. See the module docs for why, and for what the skip
    /// costs a request whose response was in the discarded frame.
    ///
    /// Cancel-safe: the client's message loop polls this inside a `select!`,
    /// and bytes read before a lost race stay buffered for the next call.
    /// Skipping happens between reads, so a cancellation can only lose a
    /// frame that was going to be discarded anyway.
    async fn recv(&mut self) -> Result<Option<String>> {
        loop {
            let Some(frame) = self
                .stdout
                .next_frame()
                .await
                .map_err(|e| Error::Transport(format!("Failed to read: {}", e)))?
            else {
                return Ok(None);
            };

            match frame {
                InputFrame::Line(line) => {
                    return Ok(Some(clean_input_line(&line).to_string()));
                }
                InputFrame::Undecodable => {
                    tracing::warn!(
                        "invalid UTF-8 in a frame from the server, discarding it; \
                         a response carried in it will not arrive and its request \
                         will wait for its timeout"
                    );
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[tokio::test]
    async fn test_spawn_nonexistent_program() {
        let result = StdioClientTransport::spawn("nonexistent-program-xyz", &[]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_and_recv_via_cat() {
        // `cat` echoes stdin to stdout line-by-line
        let mut transport = StdioClientTransport::spawn("cat", &[]).await.unwrap();

        assert!(transport.is_connected());

        // Send a JSON message
        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"test"}"#;
        transport.send(msg).await.unwrap();

        // cat echoes it back
        let received = transport.recv().await.unwrap();
        assert_eq!(received.as_deref(), Some(msg));
    }

    #[tokio::test]
    async fn test_close_signals_eof() {
        let mut transport = StdioClientTransport::spawn("cat", &[]).await.unwrap();
        assert!(transport.is_connected());

        transport.close().await.unwrap();
        assert!(!transport.is_connected());
    }

    #[tokio::test]
    async fn test_recv_returns_none_on_eof() {
        // `true` exits immediately with no output
        let mut transport = StdioClientTransport::spawn("true", &[]).await.unwrap();

        // Should get None (EOF) since `true` produces no output and exits
        let result = transport.recv().await.unwrap();
        assert_eq!(result, None);
    }

    // =========================================================================
    // Output that will not decode (#1296)
    //
    // A stray byte on the server's stdout used to surface as `InvalidData`,
    // which `recv` turned into `Error::Transport`. The client's message loop
    // treats `Err` as the connection being over: it breaks and fails every
    // pending request. These pin that one bad frame now costs one frame.
    // =========================================================================

    /// Spawn a shell that writes exactly `script` to stdout.
    ///
    /// Octal escapes keep `printf` portable across the shells CI runs.
    async fn spawn_writer(script: &str) -> StdioClientTransport {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        StdioClientTransport::spawn_command(&mut cmd).await.unwrap()
    }

    #[tokio::test]
    async fn an_undecodable_frame_is_skipped_and_the_next_one_is_delivered() {
        let response = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let mut transport =
            spawn_writer(r#"printf '\377\376\n'; printf '{"jsonrpc":"2.0","id":1,"result":{}}\n'"#)
                .await;

        assert_eq!(
            transport.recv().await.unwrap().as_deref(),
            Some(response),
            "the frame after a bad one must still be delivered"
        );
    }

    #[tokio::test]
    async fn repeated_undecodable_frames_do_not_end_the_transport() {
        let response = r#"{"jsonrpc":"2.0","id":7,"result":{}}"#;
        let mut transport = spawn_writer(
            r#"printf '\377\n\376\n\377\376\n'; printf '{"jsonrpc":"2.0","id":7,"result":{}}\n'"#,
        )
        .await;

        assert_eq!(transport.recv().await.unwrap().as_deref(), Some(response));
    }

    #[tokio::test]
    async fn an_undecodable_frame_before_eof_reports_eof_rather_than_an_error() {
        // A server whose last output does not decode has still closed
        // cleanly. Reporting an error here would fail pending requests that
        // EOF handling is supposed to close out normally.
        let mut transport = spawn_writer(r#"printf '\377\376\n'"#).await;

        assert_eq!(transport.recv().await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_undecodable_final_frame_without_a_newline_reports_eof() {
        let mut transport = spawn_writer(r#"printf '\377\376'"#).await;

        assert_eq!(transport.recv().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_partial_frame_survives_a_cancelled_receive() {
        // The client's message loop polls `recv` inside a `select!`, so a
        // frame that loses the race has to survive to the next call rather
        // than arriving split in two.
        let frame = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#;
        let mut transport = spawn_writer(
            r#"printf '{"jsonrpc":"2.0","id":2,'; sleep 0.2; printf '"result":{"tools":[]}}\n'"#,
        )
        .await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), transport.recv())
                .await
                .is_err(),
            "a partial frame must remain pending until its newline arrives"
        );

        assert_eq!(transport.recv().await.unwrap().as_deref(), Some(frame));
    }

    #[tokio::test]
    async fn test_send_after_close_fails() {
        let mut transport = StdioClientTransport::spawn("cat", &[]).await.unwrap();
        transport.close().await.unwrap();

        let result = transport.send("hello").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_spawn_command_with_env() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo $TEST_VAR"]);
        cmd.env("TEST_VAR", "hello_from_test");

        let mut transport = StdioClientTransport::spawn_command(&mut cmd).await.unwrap();

        let received = transport.recv().await.unwrap();
        assert_eq!(received.as_deref(), Some("hello_from_test"));
    }

    #[tokio::test]
    async fn test_spawn_command_preserves_piped_stderr() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo diagnostic >&2"]);
        cmd.stderr(Stdio::piped());

        let mut transport = StdioClientTransport::spawn_command(&mut cmd).await.unwrap();
        let stderr = transport
            .take_stderr()
            .expect("spawn_command must not replace piped stderr");
        let mut stderr = BufReader::new(stderr);
        let mut line = String::new();
        stderr.read_line(&mut line).await.unwrap();

        assert_eq!(line.trim(), "diagnostic");
        assert!(transport.take_stderr().is_none());
    }

    /// #1303: the server strips a BOM before parsing and the client did not,
    /// so the same frame was handled on one end of the connection and dropped
    /// on the other. The frame most likely to carry one is the `initialize`
    /// response, whose loss leaves the handshake waiting for a timeout.
    #[tokio::test]
    async fn test_recv_strips_a_leading_bom() {
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            r#"printf '\357\273\277{"jsonrpc":"2.0","id":1,"result":{}}\n'"#,
        ]);

        let mut transport = StdioClientTransport::spawn_command(&mut cmd).await.unwrap();
        let frame = transport
            .recv()
            .await
            .unwrap()
            .expect("the frame must arrive");

        assert_eq!(frame, r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        serde_json::from_str::<serde_json::Value>(&frame)
            .expect("and it must parse, which is the point");
    }

    #[tokio::test]
    async fn test_multiple_send_recv_roundtrips() {
        let mut transport = StdioClientTransport::spawn("cat", &[]).await.unwrap();

        for i in 0..5 {
            let msg = format!(r#"{{"id":{i},"msg":"test"}}"#);
            transport.send(&msg).await.unwrap();
            let received = transport.recv().await.unwrap();
            assert_eq!(received.as_deref(), Some(msg.as_str()));
        }

        transport.close().await.unwrap();
    }
}
