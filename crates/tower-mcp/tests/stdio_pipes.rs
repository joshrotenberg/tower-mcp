//! `StdioTransport::run()` over real pipes and sockets, in a child process.
//!
//! `run()` reads fd 0 and writes fd 1. Unit tests cannot stand in for that
//! (`run_with_streams` skips the code that picks the stream types), and a test
//! process's own stdio is captured by the harness. So this test re-executes
//! itself: with `STDIO_PIPES_SERVER` set it is an MCP server speaking over its
//! standard streams, otherwise it is the client that spawns that server over
//! pipes, over socketpairs (what Node's `child_process` provides), and over
//! files, in each [`StdioPipes`] mode.
//!
//! It has its own `main` (`harness = false`) because libtest writes to stdout,
//! which in the server role is the protocol stream.

#[cfg(not(unix))]
fn main() {}

#[cfg(unix)]
fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    match std::env::var("STDIO_PIPES_SERVER") {
        Ok(mode) => {
            runtime.block_on(unix::server(&mode));
            // Dropping the runtime waits for blocking-pool threads, which is
            // the behaviour under test: do it explicitly and visibly.
            drop(runtime);
        }
        Err(_) => runtime.block_on(unix::drive()),
    }
}

#[cfg(unix)]
mod unix {
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
    use tokio::process::{Child, Command};
    use tokio::time::timeout;
    use tower_mcp::{CallToolResult, McpRouter, NoParams, StdioPipes, StdioTransport, ToolBuilder};

    const MODES: [(&str, StdioPipes); 3] = [
        ("blocking", StdioPipes::Blocking),
        ("auto", StdioPipes::Auto),
        ("shared", StdioPipes::Shared),
    ];

    // ------------------------------------------------------------------------
    // Server role
    // ------------------------------------------------------------------------

    fn nonblocking(fd: i32) -> bool {
        // SAFETY: `F_GETFL` only reads the descriptor's status flags.
        unsafe { libc::fcntl(fd, libc::F_GETFL) & libc::O_NONBLOCK != 0 }
    }

    pub async fn server(mode: &str) {
        let mode = MODES.iter().find(|(name, _)| *name == mode).unwrap().1;
        // Reports whether fd 0 and fd 1 are non-blocking while `run()` is up.
        let flags = ToolBuilder::new("flags")
            .description("O_NONBLOCK on stdin and stdout")
            .handler(|_: NoParams| async {
                let text = format!("{}{}", nonblocking(0) as u8, nonblocking(1) as u8);
                Ok(CallToolResult::text(text))
            })
            .build();
        let router = McpRouter::new()
            .server_info("stdio-pipes", "0.0.0")
            .tool(flags);
        let mut transport = StdioTransport::new(router).pipes(mode);

        if let Ok(ms) = std::env::var("STDIO_PIPES_STOP_AFTER_MS") {
            let handle = transport.handle();
            let ms = ms.parse().unwrap();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                handle.shutdown().unwrap();
            });
        }

        transport.run().await.unwrap();
        // `run()` has dropped its streams: whatever it set is undone, and the
        // protocol stream is over, so stdout is free for a marker. Use the
        // raw descriptor, since std's `println!` would panic on EAGAIN if a
        // leaked O_NONBLOCK left stdout non-blocking and the pipe full.
        let marker = format!("done {}{}\n", nonblocking(0) as u8, nonblocking(1) as u8);
        // SAFETY: writes a valid buffer to fd 1.
        unsafe { libc::write(1, marker.as_ptr().cast(), marker.len()) };
    }

    // ------------------------------------------------------------------------
    // Client role
    // ------------------------------------------------------------------------

    #[derive(Clone, Copy, Debug)]
    enum Flavor {
        Pipe,
        Socket,
    }

    struct Server {
        child: Child,
        input: Option<Box<dyn AsyncWrite + Unpin + Send>>,
        output: Box<dyn AsyncBufRead + Unpin + Send>,
    }

    fn command(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env("STDIO_PIPES_SERVER", mode)
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        command
    }

    fn spawn(flavor: Flavor, mode: &str, stop_after_ms: Option<u64>) -> Server {
        let mut command = command(mode);
        if let Some(ms) = stop_after_ms {
            command.env("STDIO_PIPES_STOP_AFTER_MS", ms.to_string());
        }
        match flavor {
            Flavor::Pipe => {
                command.stdin(Stdio::piped()).stdout(Stdio::piped());
                let mut child = command.spawn().unwrap();
                let input = child.stdin.take().unwrap();
                let output = BufReader::new(child.stdout.take().unwrap());
                Server {
                    child,
                    input: Some(Box::new(input)),
                    output: Box::new(output),
                }
            }
            Flavor::Socket => {
                use std::os::unix::net::UnixStream;
                let (stdin_parent, stdin_child) = UnixStream::pair().unwrap();
                let (stdout_parent, stdout_child) = UnixStream::pair().unwrap();
                command
                    .stdin(Stdio::from(std::os::fd::OwnedFd::from(stdin_child)))
                    .stdout(Stdio::from(std::os::fd::OwnedFd::from(stdout_child)));
                let child = command.spawn().unwrap();
                drop(command);
                stdin_parent.set_nonblocking(true).unwrap();
                stdout_parent.set_nonblocking(true).unwrap();
                let input = tokio::net::UnixStream::from_std(stdin_parent).unwrap();
                let output = tokio::net::UnixStream::from_std(stdout_parent).unwrap();
                Server {
                    child,
                    input: Some(Box::new(input)),
                    output: Box::new(BufReader::new(output)),
                }
            }
        }
    }

    impl Server {
        async fn send(&mut self, line: &str) {
            let input = self.input.as_mut().unwrap();
            input.write_all(line.as_bytes()).await.unwrap();
            input.write_all(b"\n").await.unwrap();
        }

        async fn recv(&mut self) -> String {
            let mut line = String::new();
            let n = timeout(Duration::from_secs(10), self.output.read_line(&mut line))
                .await
                .expect("timed out waiting for the server")
                .unwrap();
            assert!(n > 0, "server closed its output early");
            line
        }

        async fn handshake(&mut self) {
            self.send(
                r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
            )
            .await;
            assert!(self.recv().await.contains("\"serverInfo\""));
            self.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .await;
        }

        async fn call_flags(&mut self, id: u64) -> String {
            self.send(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"flags","arguments":{{}}}}}}"#
            ))
            .await;
            let response: serde_json::Value = serde_json::from_str(&self.recv().await).unwrap();
            assert_eq!(response["id"], id);
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        }

        /// Close the server's stdin and return the marker it prints after
        /// `run()`, once the process has exited cleanly.
        async fn finish(mut self) -> String {
            drop(self.input.take());
            self.finish_after_stop().await
        }

        /// Read the marker and wait for exit without closing stdin first.
        async fn finish_after_stop(&mut self) -> String {
            let mut marker = String::new();
            while !marker.starts_with("done") {
                marker = self.recv().await;
            }
            let status = timeout(Duration::from_secs(10), self.child.wait())
                .await
                .expect("the server did not exit")
                .unwrap();
            assert!(status.success());
            marker.trim().trim_start_matches("done ").to_string()
        }
    }

    /// What `flags` should report while `run()` is up: the transport is
    /// non-blocking on the shared descriptors only in `Shared` mode, and only
    /// where it cannot reopen a private description (everything but a Linux
    /// pipe). A private reopen leaves fd 0 and fd 1 blocking.
    fn expected_flags(flavor: Flavor, mode: &str) -> &'static str {
        match (mode, flavor, cfg!(target_os = "linux")) {
            ("shared", Flavor::Socket, _) => "11",
            ("shared", Flavor::Pipe, false) => "11",
            _ => "00",
        }
    }

    pub async fn drive() {
        for flavor in [Flavor::Pipe, Flavor::Socket] {
            for (mode, _) in MODES {
                concurrent_calls(flavor, mode).await;
                stop_with_stdin_open(flavor, mode).await;
                println!("ok {flavor:?} {mode}");
            }
        }
        piped_stdio_is_not_required().await;
        println!("ok files");
    }

    /// Many requests in flight at once, answered by id, with the descriptors
    /// restored once the transport ends.
    async fn concurrent_calls(flavor: Flavor, mode: &str) {
        let mut server = spawn(flavor, mode, None);
        server.handshake().await;
        assert_eq!(
            server.call_flags(1).await,
            expected_flags(flavor, mode),
            "{flavor:?} {mode}"
        );

        for id in 100..300 {
            server
                .send(&format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"flags","arguments":{{}}}}}}"#
                ))
                .await;
        }
        let mut seen = std::collections::BTreeSet::new();
        for _ in 100..300 {
            let response: serde_json::Value = serde_json::from_str(&server.recv().await).unwrap();
            seen.insert(response["id"].as_u64().unwrap());
        }
        assert_eq!(seen.len(), 200);

        // Nothing left over once the transport has finished.
        assert_eq!(server.finish().await, "00", "{flavor:?} {mode}");
    }

    /// Stopping the transport while the client keeps stdin open must not leave
    /// the server stuck on a read it cannot cancel, in the modes that read
    /// through the reactor.
    async fn stop_with_stdin_open(flavor: Flavor, mode: &str) {
        let reactor = match mode {
            "shared" => true,
            "auto" => cfg!(target_os = "linux") && matches!(flavor, Flavor::Pipe),
            _ => false,
        };
        if !reactor {
            return;
        }
        let mut server = spawn(flavor, mode, Some(150));
        server.handshake().await;
        // `finish_after_stop` waits for exit with stdin still open.
        assert_eq!(server.finish_after_stop().await, "00", "{flavor:?} {mode}");
        drop(server);
    }

    /// Files are neither pipes nor sockets: `run()` must keep using tokio's
    /// handles, and not try to put a regular file in the reactor.
    async fn piped_stdio_is_not_required() {
        let dir =
            std::env::temp_dir().join(format!("tower-mcp-stdio-pipes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input_path = dir.join("input");
        let output_path = dir.join("output");
        std::fs::write(
            &input_path,
            concat!(
                r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"flags","arguments":{}}}"#,
                "\n",
            ),
        )
        .unwrap();

        for (mode, _) in MODES {
            let output = std::fs::File::create(&output_path).unwrap();
            let status = timeout(
                Duration::from_secs(10),
                command(mode)
                    .stdin(std::fs::File::open(&input_path).unwrap())
                    .stdout(output)
                    .status(),
            )
            .await
            .expect("the server did not exit")
            .unwrap();
            assert!(status.success(), "{mode}");

            let text = std::fs::read_to_string(&output_path).unwrap();
            assert!(text.contains(r#""id":7"#), "{mode}: {text}");
            // Regular files stay blocking, and so does `done` after the run.
            assert!(text.contains(r#""text":"00""#), "{mode}: {text}");
            assert!(text.contains("done 00"), "{mode}: {text}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
