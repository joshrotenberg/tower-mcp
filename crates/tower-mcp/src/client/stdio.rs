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
//! # Stopping the child
//!
//! [`ClientTransport::close`] drops the child's stdin and waits for it to
//! exit on its own, then kills it. The wait defaults to 5 seconds and is set
//! with [`StdioClientTransport::close_timeout`]. A kill signals only the
//! child itself, so a server launched through `npx` or a shell wrapper can
//! leave descendants running. On Unix, a child spawned as its own process
//! group leader can be stopped together with its descendants through
//! [`StdioClientTransport::process_group_shutdown`]. How the child exited is
//! available from [`StdioClientTransport::exit_status`] after `close`.
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

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

use super::transport::ClientTransport;
use crate::error::{Error, Result};
use crate::framing::{FrameReader, InputFrame, clean_input_line};

/// How long `close` waits at each stage of stopping the child unless
/// [`StdioClientTransport::close_timeout`] says otherwise.
const DEFAULT_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the process group is checked for stragglers while `close`
/// waits for it to empty.
#[cfg(unix)]
const GROUP_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Client transport that communicates with a subprocess via stdio.
///
/// Spawns a child process and communicates using line-delimited JSON-RPC
/// messages over stdin (write) and stdout (read). By default stderr is
/// inherited so server debug output appears in the client's terminal. A
/// caller using [`Self::spawn_command`] may redirect or pipe it instead.
///
/// # Stopping the server
///
/// [`ClientTransport::close`] closes the child's stdin, waits up to
/// [`close_timeout`](Self::close_timeout) (default 5 seconds) for it to exit,
/// and kills it if it does not. The child's exit status is then available
/// from [`exit_status`](Self::exit_status). A transport dropped without
/// `close` kills the child.
///
/// Killing signals the child only. Servers started through `npx` or a shell
/// wrapper often have descendants that keep running after their parent dies.
/// To stop those as well on Unix, spawn the child as the leader of its own
/// process group and opt in to [`process_group_shutdown`](Self::process_group_shutdown):
///
/// ```rust,no_run
/// use std::time::Duration;
/// use tokio::process::Command;
/// use tower_mcp::client::StdioClientTransport;
///
/// # #[cfg(unix)]
/// # async fn example() -> Result<(), tower_mcp::BoxError> {
/// let mut cmd = Command::new("npx");
/// cmd.args(["-y", "@modelcontextprotocol/server-github"])
///    .process_group(0);
/// let transport = StdioClientTransport::spawn_command(&mut cmd)
///     .await?
///     .close_timeout(Duration::from_secs(2))
///     .process_group_shutdown(true);
/// # let _ = transport;
/// # Ok(())
/// # }
/// ```
pub struct StdioClientTransport {
    child: Option<Child>,
    /// The child's pid, kept after `close` so callers can still log it.
    pid: Option<u32>,
    exit_status: Option<ExitStatus>,
    close_timeout: Duration,
    // Only read on Unix; other platforms accept and ignore the setting.
    #[cfg_attr(not(unix), allow(dead_code))]
    process_group_shutdown: bool,
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
    /// The child is killed if the transport is dropped before
    /// [`ClientTransport::close`] has stopped it.
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

        // Tie the child's lifetime to this transport. `close()` is the
        // graceful path, but nothing guarantees it runs: a caller can drop
        // the transport on an error path, for example after giving up on a
        // slow `initialize`, and `Drop for McpClient` aborts the message loop
        // task before its cleanup can await `close()`. Without this the child
        // outlives every handle to it, still running with all of its side
        // effects and with nothing left that can stop it.
        //
        // `kill_on_drop` also hands the pid to tokio's orphan reaper, so no
        // zombie is left behind. A clean `close()` takes the child out of
        // `self` first, so this never double-kills.
        cmd.kill_on_drop(true);

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

        Ok(Self::from_parts(child, stdin, stdout))
    }

    fn from_parts(
        child: Child,
        stdin: tokio::process::ChildStdin,
        stdout: tokio::process::ChildStdout,
    ) -> Self {
        Self {
            pid: child.id(),
            child: Some(child),
            exit_status: None,
            close_timeout: DEFAULT_CLOSE_TIMEOUT,
            process_group_shutdown: false,
            stdin: Some(stdin),
            stdout: FrameReader::with_max_len(
                stdout,
                crate::framing::DEFAULT_MAX_RESPONSE_FRAME_LEN,
            ),
        }
    }

    /// Cap how many bytes one newline-delimited frame from the server may
    /// buffer before a delimiter arrives.
    ///
    /// A server that writes bytes without ever sending `\n` would otherwise
    /// grow the frame buffer without bound. This matters most here: a client
    /// may launch an untrusted server binary, unlike the server side where
    /// the parent process is usually trusted. Once the cap is crossed,
    /// [`ClientTransport::recv`] fails with a transport error, the same
    /// outcome any other stdout read failure already produces. Default:
    /// 16 MiB, the same per-message cap the HTTP client applies to SSE events.
    ///
    /// ```rust,no_run
    /// use tower_mcp::client::StdioClientTransport;
    ///
    /// # async fn example() -> Result<(), tower_mcp::BoxError> {
    /// let transport = StdioClientTransport::spawn("my-mcp-server", &[])
    ///     .await?
    ///     .max_frame_len(1024 * 1024);
    /// # let _ = transport;
    /// # Ok(())
    /// # }
    /// ```
    pub fn max_frame_len(mut self, bytes: usize) -> Self {
        self.stdout.set_max_len(bytes);
        self
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

        Ok(Self::from_parts(child, stdin, stdout))
    }

    /// Set how long [`ClientTransport::close`] waits at each stage of
    /// stopping the child. Default: 5 seconds.
    ///
    /// `close` first closes the child's stdin and waits this long for it to
    /// exit on its own. If it has not, it is killed, or, with
    /// [`process_group_shutdown`](Self::process_group_shutdown), its process
    /// group is sent `SIGTERM` and given this long again before `SIGKILL`.
    ///
    /// ```rust,no_run
    /// use std::time::Duration;
    /// use tower_mcp::client::StdioClientTransport;
    ///
    /// # async fn example() -> Result<(), tower_mcp::BoxError> {
    /// let transport = StdioClientTransport::spawn("my-mcp-server", &[])
    ///     .await?
    ///     .close_timeout(Duration::from_millis(500));
    /// # let _ = transport;
    /// # Ok(())
    /// # }
    /// ```
    pub fn close_timeout(mut self, timeout: Duration) -> Self {
        self.close_timeout = timeout;
        self
    }

    /// Stop the child's whole process group when closing, not only the child.
    ///
    /// Killing a child signals that process alone, so descendants it started
    /// (an `npx` launcher's real server, a shell wrapper's command) keep
    /// running. With this enabled, [`ClientTransport::close`] stops the group:
    ///
    /// 1. Close the child's stdin and wait up to
    ///    [`close_timeout`](Self::close_timeout) for the child to exit.
    /// 2. Send `SIGTERM` to the process group. This also reaches descendants
    ///    that outlived a child which exited cleanly in step 1; if none are
    ///    left, there is nothing to wait for.
    /// 3. Wait up to `close_timeout` for the child to be reaped and the group
    ///    to empty.
    /// 4. Send `SIGKILL` to whatever remains in the group.
    ///
    /// The group is only signalled if the child is the leader of its own
    /// group, which is the case when the [`Command`] passed to
    /// [`spawn_command`](Self::spawn_command) called `process_group(0)`.
    /// A child that merely shares this process's group is never signalled
    /// as a group, because that would signal this process too; `close` falls
    /// back to killing the child alone and logs a warning.
    ///
    /// A child in its own process group no longer receives terminal signals
    /// such as the `SIGINT` from Ctrl-C, which go to the foreground group.
    ///
    /// Off by default. On non-Unix platforms this setting has no effect and
    /// `close` behaves as described on [`close_timeout`](Self::close_timeout).
    pub fn process_group_shutdown(mut self, enabled: bool) -> Self {
        self.process_group_shutdown = enabled;
        self
    }

    /// The child's process id.
    ///
    /// Still returned after [`ClientTransport::close`], when the process no
    /// longer exists and the number may have been reused. Returns `None` only
    /// if the operating system never reported a pid.
    pub fn child_id(&self) -> Option<u32> {
        self.pid
    }

    /// How the child exited, as recorded by [`ClientTransport::close`].
    ///
    /// `None` before `close`, and if the exit status could not be read. After
    /// a kill, the status reports the signal rather than a code (on Unix,
    /// [`std::os::unix::process::ExitStatusExt::signal`]).
    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.exit_status
    }
}

/// Process-group primitives for `close`. The group id is the leader's pid.
#[cfg(unix)]
mod pgroup {
    use libc::{c_int, pid_t};

    /// The group id for `pid` if it leads a group of its own.
    ///
    /// The guard against signalling the caller's own group: a child that did
    /// not request `process_group(0)` shares it, and `killpg` on it would
    /// signal this process as well.
    pub(super) fn own_group_of(pid: u32) -> Option<pid_t> {
        let pid = pid_t::try_from(pid).ok()?;
        // SAFETY: `getpgid` takes a plain integer and touches no memory.
        (unsafe { libc::getpgid(pid) } == pid).then_some(pid)
    }

    /// Send `signal` to every process in the group. False if it reached
    /// none, including when the group is already empty.
    pub(super) fn signal(pgid: pid_t, signal: c_int) -> bool {
        // SAFETY: `killpg` takes plain integers and touches no memory.
        unsafe { libc::killpg(pgid, signal) == 0 }
    }
}

/// Stop the process group led by `child` once its graceful exit has been
/// waited on: `SIGTERM`, wait, `SIGKILL`.
///
/// `status` is filled in if the leader is reaped along the way. If the leader
/// was reaped before this ran, `status` is already set.
#[cfg(unix)]
async fn stop_process_group(
    child: &mut Child,
    pgid: libc::pid_t,
    grace: Duration,
    status: &mut Option<ExitStatus>,
) {
    use tokio::time::{Instant, sleep, timeout};

    if !pgroup::signal(pgid, libc::SIGTERM) {
        // Nobody left in the group.
        return;
    }

    let deadline = Instant::now() + grace;
    loop {
        if status.is_none() {
            // The leader stays in the group as a zombie until it is reaped,
            // so the group cannot read as empty before this.
            if let Ok(Some(exit)) = child.try_wait() {
                *status = Some(exit);
            }
        }
        if status.is_some() && !pgroup::signal(pgid, 0) {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        sleep(GROUP_POLL_INTERVAL).await;
    }

    tracing::warn!("Process group did not exit after SIGTERM, sending SIGKILL");
    pgroup::signal(pgid, libc::SIGKILL);

    if status.is_none() {
        match timeout(grace, child.wait()).await {
            Ok(Ok(exit)) => *status = Some(exit),
            Ok(Err(e)) => tracing::error!(error = %e, "Error waiting for child"),
            Err(_) => tracing::error!("Child did not exit after SIGKILL"),
        }
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

    /// Stop the child: close its stdin, wait for it to exit, then kill it.
    ///
    /// See [`close_timeout`](StdioClientTransport::close_timeout) and
    /// [`process_group_shutdown`](StdioClientTransport::process_group_shutdown)
    /// for the wait and for stopping descendants. The exit status is recorded
    /// for [`exit_status`](StdioClientTransport::exit_status).
    async fn close(&mut self) -> Result<()> {
        // Resolved before stdin is dropped, while the child is certain to
        // still be in its group.
        #[cfg(unix)]
        let group = if self.process_group_shutdown {
            let group = self.pid.and_then(pgroup::own_group_of);
            if group.is_none() {
                tracing::warn!(
                    "process group shutdown is enabled but the child does not lead its own \
                     process group (spawn it with `process_group(0)`); stopping the child only"
                );
            }
            group
        } else {
            None
        };
        // No process groups to signal here; keeps the code below uniform.
        #[cfg(not(unix))]
        let group: Option<()> = None;

        // Drop stdin to signal EOF to the child process
        self.stdin.take();

        if let Some(mut child) = self.child.take() {
            let result = tokio::time::timeout(self.close_timeout, child.wait()).await;

            match result {
                Ok(Ok(status)) => {
                    tracing::info!(status = ?status, "Child process exited");
                    self.exit_status = Some(status);
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "Error waiting for child");
                }
                Err(_) if group.is_some() => {
                    // The group stop below sends the signals.
                    tracing::warn!("Timeout waiting for child, stopping its process group");
                }
                Err(_) => {
                    tracing::warn!("Timeout waiting for child, killing");
                    let _ = child.kill().await;
                    self.exit_status = child.try_wait().ok().flatten();
                }
            }

            // Runs after a clean exit too: descendants can outlive a leader
            // that left when its stdin closed.
            #[cfg(unix)]
            if let Some(pgid) = group {
                stop_process_group(&mut child, pgid, self.close_timeout, &mut self.exit_status)
                    .await;
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

    /// Dropping the transport without `close()` must still kill the child.
    ///
    /// The paths that skip `close()` are real: a caller that gives up on a
    /// slow `initialize` drops the transport, and `Drop for McpClient` aborts
    /// the message loop task that would have awaited `close()`. A surviving
    /// child is unreachable but keeps running.
    ///
    /// Liveness is measured by side effect rather than by pid: a killed
    /// process stops appending to its file, whereas `kill -0` on a
    /// not-yet-reaped zombie still succeeds and would pass a broken
    /// implementation.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_transport_kills_the_child() {
        use std::time::{Duration, Instant};

        let ticks = std::env::temp_dir().join(format!(
            "tower-mcp-kill-on-drop-{}-{}.ticks",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&ticks);

        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            // Ignore SIGTERM so the test cannot pass on a polite signal that a
            // real unresponsive server would also ignore.
            &format!(
                "trap '' TERM; while :; do printf x >> '{}'; sleep 0.1; done",
                ticks.display()
            ),
        ]);
        let transport = StdioClientTransport::spawn_command(&mut cmd).await.unwrap();

        // Wait until the child is demonstrably alive and writing.
        let deadline = Instant::now() + Duration::from_secs(10);
        let size = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        while size(&ticks) == 0 {
            assert!(
                Instant::now() < deadline,
                "child never started writing; the test cannot tell life from death"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        drop(transport);

        // Let the kill land, then prove the writing stopped for good.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let settled = size(&ticks);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let later = size(&ticks);

        let _ = std::fs::remove_file(&ticks);
        assert_eq!(
            settled, later,
            "child kept running after its transport was dropped ({settled} -> {later} bytes)"
        );
    }

    // =========================================================================
    // Stopping the child (#1549)
    // =========================================================================

    #[cfg(unix)]
    fn scratch_file(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tower-mcp-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Wait for a script to write its background pid to `path`.
    #[cfg(unix)]
    async fn read_pid(path: &std::path::Path) -> i32 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child never wrote the background pid"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Whether `pid` is gone, allowing time for it to be signalled and
    /// reaped. The processes checked here are reparented to init when their
    /// shell dies, so a killed one stops answering `kill(pid, 0)`; a zombie
    /// would still answer, which is why this waits instead of checking once.
    #[cfg(unix)]
    async fn is_gone(pid: i32) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            // SAFETY: signal 0 only checks that the process exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }

    /// Kill `pid` so a failed assertion does not leave a `sleep` behind.
    #[cfg(unix)]
    fn reap_leftover(pid: i32) {
        // SAFETY: a plain signal to a pid this test started.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    /// A child that starts a background process and ignores stdin EOF is
    /// stopped together with that process when its group is signalled. Without
    /// `process_group_shutdown` the background `sleep` outlives `close` (the
    /// case in #1549), so this fails if the group is not signalled.
    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_shutdown_stops_the_childs_descendants() {
        use std::time::{Duration, Instant};

        let pidfile = scratch_file("pgroup-pid");
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!("sleep 60 & echo $! > '{}'; sleep 60", pidfile.display()),
        ])
        .process_group(0);
        let mut transport = StdioClientTransport::spawn_command(&mut cmd)
            .await
            .unwrap()
            .close_timeout(Duration::from_millis(300))
            .process_group_shutdown(true);
        let background = read_pid(&pidfile).await;

        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(10), transport.close())
            .await
            .expect("close must not hang")
            .unwrap();
        let elapsed = started.elapsed();

        let gone = is_gone(background).await;
        reap_leftover(background);
        let _ = std::fs::remove_file(&pidfile);
        assert!(gone, "the background process outlived close");
        assert!(
            elapsed < Duration::from_secs(3),
            "SIGTERM should end the group without waiting out SIGKILL, took {elapsed:?}"
        );
        assert!(!transport.exit_status().unwrap().success());
    }

    /// Descendants can outlive a child that exits cleanly when its stdin
    /// closes. The group is still signalled, and the clean status is kept.
    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_shutdown_stops_descendants_of_a_child_that_exited() {
        use std::time::Duration;

        let pidfile = scratch_file("pgroup-clean-pid");
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!(
                "sleep 60 & echo $! > '{}'; read line; exit 0",
                pidfile.display()
            ),
        ])
        .process_group(0);
        let mut transport = StdioClientTransport::spawn_command(&mut cmd)
            .await
            .unwrap()
            .process_group_shutdown(true);
        let background = read_pid(&pidfile).await;

        tokio::time::timeout(Duration::from_secs(10), transport.close())
            .await
            .expect("close must not hang")
            .unwrap();

        let gone = is_gone(background).await;
        reap_leftover(background);
        let _ = std::fs::remove_file(&pidfile);
        assert!(gone, "the background process outlived a clean exit");
        assert_eq!(transport.exit_status().unwrap().code(), Some(0));
    }

    /// Process group shutdown on a child that did not call `process_group(0)`
    /// must not signal the group it shares with this process. If it did, this
    /// test process would be signalled and die.
    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_shutdown_ignores_a_child_in_the_callers_group() {
        use std::time::Duration;

        let mut cmd = Command::new("sh");
        cmd.args(["-c", "while :; do sleep 0.1; done"]);
        let mut transport = StdioClientTransport::spawn_command(&mut cmd)
            .await
            .unwrap()
            .close_timeout(Duration::from_millis(200))
            .process_group_shutdown(true);

        tokio::time::timeout(Duration::from_secs(10), transport.close())
            .await
            .expect("close must not hang")
            .unwrap();
        assert!(!transport.exit_status().unwrap().success());
    }

    /// `close_timeout` bounds the wait on a child that ignores stdin EOF.
    #[cfg(unix)]
    #[tokio::test]
    async fn close_timeout_bounds_the_wait_for_the_child() {
        use std::os::unix::process::ExitStatusExt;
        use std::time::{Duration, Instant};

        let mut cmd = Command::new("sh");
        cmd.args(["-c", "while :; do sleep 0.1; done"]);
        let mut transport = StdioClientTransport::spawn_command(&mut cmd)
            .await
            .unwrap()
            .close_timeout(Duration::from_millis(200));

        let started = Instant::now();
        transport.close().await.unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(1),
            "close should wait the configured 200ms, then kill; took {elapsed:?}"
        );
        assert_eq!(
            transport.exit_status().unwrap().signal(),
            Some(libc::SIGKILL)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exit_status_is_readable_after_close() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "read line; exit 3"]);
        let mut transport = StdioClientTransport::spawn_command(&mut cmd).await.unwrap();

        assert!(transport.exit_status().is_none());
        transport.close().await.unwrap();

        assert_eq!(transport.exit_status().unwrap().code(), Some(3));
    }

    #[tokio::test]
    async fn child_id_is_available_before_and_after_close() {
        let mut transport = StdioClientTransport::spawn("cat", &[]).await.unwrap();
        let pid = transport.child_id().expect("a spawned child has a pid");

        transport.close().await.unwrap();

        assert_eq!(transport.child_id(), Some(pid));
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

    // =========================================================================
    // Bounded frame length (#1470)
    //
    // The client matters most for this bound: it may launch an untrusted
    // server binary, whose stdout has no reason to ever send a newline.
    // =========================================================================

    #[tokio::test]
    async fn max_frame_len_rejects_a_server_that_never_sends_a_delimiter() {
        // Pure POSIX shell builtins, no external tools: the server writes
        // forever and never sends a newline. Without a bound this would hang
        // rather than fail, which is what the fix (#1470) prevents; the
        // `timeout` below turns a regression into a fast failure instead of
        // an unbounded test hang. `close()` reaps the never-ending child
        // regardless of how the assertion below turns out.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "while true; do printf 'aaaaaaaaaa'; done"]);
        let mut transport = StdioClientTransport::spawn_command(&mut cmd)
            .await
            .unwrap()
            .max_frame_len(1024);

        let result = tokio::time::timeout(std::time::Duration::from_secs(10), transport.recv())
            .await
            .expect("an oversized frame must fail rather than hang waiting for a delimiter");

        assert!(
            result.is_err(),
            "a frame past the configured limit must surface as an error"
        );

        transport.close().await.ok();
    }

    /// A frame comfortably under a small configured limit still arrives.
    #[tokio::test]
    async fn max_frame_len_does_not_affect_a_frame_under_the_limit() {
        let mut transport = StdioClientTransport::spawn("cat", &[])
            .await
            .unwrap()
            .max_frame_len(4096);

        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"test"}"#;
        transport.send(msg).await.unwrap();

        let received = transport.recv().await.unwrap();
        assert_eq!(received.as_deref(), Some(msg));
    }
}
