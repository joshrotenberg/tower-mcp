//! Integration coverage for `UnixSocketTransport` (#1072).
//!
//! The transport delegates every protocol decision to `HttpTransport` and
//! only swaps the listener, so these tests concentrate on what is unique to
//! it: binding a socket path, the stale-socket cleanup branch, and the fact
//! that a real MCP conversation survives the Unix-domain listener. The
//! builder methods are exercised through `into_router_with_handle`, which
//! needs no listener at all.

#![cfg(all(unix, feature = "unix"))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder, UnixSocketTransport};

const PROTOCOL_VERSION: &str = "2025-11-25";

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

fn test_router() -> McpRouter {
    let add = ToolBuilder::new("add")
        .description("Add two numbers together")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    McpRouter::new()
        .server_info("unix-socket-test", "1.0.0")
        .tool(add)
}

/// A socket path inside a per-test directory under the system temp dir.
///
/// `tempfile` is deliberately avoided: on macOS `sun_path` is 104 bytes and
/// the temp dir is already long, so the name is kept short. The directory is
/// removed by [`SocketPath`]'s `Drop`.
struct SocketPath {
    dir: PathBuf,
    socket: PathBuf,
}

impl SocketPath {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("tmcp-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create socket dir");
        let socket = dir.join("s.sock");
        Self { dir, socket }
    }

    fn path(&self) -> &Path {
        &self.socket
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Spawn `serve` in the background and wait until the socket accepts a
/// connection, so tests never race the listener.
async fn serve_in_background(transport: UnixSocketTransport, path: &Path) {
    let serve_path = path.to_path_buf();
    tokio::spawn(async move {
        // The task is aborted when the test ends, so an error here only
        // matters while the test is still running.
        if let Err(e) = transport.serve(&serve_path).await {
            eprintln!("serve failed: {e}");
        }
    });

    wait_until_connectable(path).await;
}

/// Block until the socket accepts a connection, so tests never race the bind.
async fn wait_until_connectable(path: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if UnixStream::connect(path).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("socket at {} never became connectable", path.display());
}

/// One raw HTTP/1.1 POST over the Unix socket, returning (status line, body).
///
/// `Connection: close` makes the response self-delimiting: the server hangs
/// up after answering, so reading to EOF is enough and no chunked or
/// keep-alive framing has to be parsed.
async fn post(path: &Path, headers: &[(&str, &str)], body: &str) -> (String, String) {
    let mut request = format!(
        "POST / HTTP/1.1\r\n\
         Host: localhost\r\n\
         Connection: close\r\n\
         Content-Type: application/json\r\n\
         Accept: application/json\r\n\
         Content-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);

    let mut stream = UnixStream::connect(path).await.expect("connect to socket");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    stream.flush().await.expect("flush request");

    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut raw))
        .await
        .expect("timed out reading response")
        .expect("read response");

    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("malformed response: {text}"));
    let status = head.lines().next().unwrap_or_default().to_string();
    (status, body.to_string())
}

/// POST a JSON-RPC frame and parse the response body as JSON.
async fn post_json(
    path: &Path,
    headers: &[(&str, &str)],
    body: &serde_json::Value,
) -> serde_json::Value {
    let (status, body) = post(path, headers, &body.to_string()).await;
    assert!(status.starts_with("HTTP/1.1 200"), "got {status}: {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("body was not JSON ({e}): {body}"))
}

fn initialize_request() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "unix-socket-test", "version": "1.0.0"}
        }
    })
}

/// The headline case: a full initialize plus tools/call round trip proving
/// the HTTP machinery works unchanged over a Unix domain socket.
#[tokio::test]
async fn initialize_and_call_a_tool_over_the_socket() {
    let socket = SocketPath::new("roundtrip");
    let transport = UnixSocketTransport::new(test_router()).disable_origin_validation();
    serve_in_background(transport, socket.path()).await;

    let init = post_json(socket.path(), &[], &initialize_request()).await;
    assert_eq!(
        init["result"]["serverInfo"]["name"], "unix-socket-test",
        "initialize did not return this server: {init}"
    );
    assert_eq!(init["result"]["protocolVersion"], PROTOCOL_VERSION);

    let call = post_json(
        socket.path(),
        &[("MCP-Protocol-Version", PROTOCOL_VERSION)],
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "add", "arguments": {"a": 10, "b": 32}}
        }),
    )
    .await;
    assert_eq!(
        call["result"]["content"][0]["text"], "42",
        "tools/call did not run the handler: {call}"
    );
}

/// `require_sessions()` must reach the HTTP session gate through this
/// constructor: a 2025-11-25 request with no session id is refused with
/// SessionRequired (-32006).
#[tokio::test]
async fn require_sessions_rejects_a_request_without_a_session_id() {
    let socket = SocketPath::new("sessions");
    let transport = UnixSocketTransport::new(test_router())
        .disable_origin_validation()
        .require_sessions();
    serve_in_background(transport, socket.path()).await;

    let response = post_json(
        socket.path(),
        &[("MCP-Protocol-Version", PROTOCOL_VERSION)],
        &serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;
    assert_eq!(
        response["error"]["code"], -32006,
        "expected SessionRequired, got: {response}"
    );
}

/// With the default `cleanup_on_bind`, a leftover file at the socket path is
/// removed rather than failing the bind. This is the crash-restart case.
#[tokio::test]
async fn serve_replaces_a_stale_socket_file() {
    let socket = SocketPath::new("stale");
    std::fs::write(socket.path(), b"stale").expect("write stale socket file");

    let transport = UnixSocketTransport::new(test_router()).disable_origin_validation();
    serve_in_background(transport, socket.path()).await;

    let init = post_json(socket.path(), &[], &initialize_request()).await;
    assert_eq!(init["result"]["serverInfo"]["name"], "unix-socket-test");
}

/// With `cleanup_on_bind(false)` the same leftover file is left alone and the
/// bind fails, surfacing a transport error instead of silently clobbering a
/// path the caller said it manages.
#[tokio::test]
async fn cleanup_on_bind_disabled_fails_on_an_existing_path() {
    let socket = SocketPath::new("nocleanup");
    std::fs::write(socket.path(), b"stale").expect("write stale socket file");

    let error = UnixSocketTransport::new(test_router())
        .cleanup_on_bind(false)
        .serve(socket.path())
        .await
        .expect_err("bind over an existing file must fail");
    let message = error.to_string();
    assert!(
        message.contains("Failed to bind Unix socket"),
        "unexpected error: {message}"
    );

    // The caller-managed file is still there.
    assert!(socket.path().exists(), "existing path must not be removed");
}

/// The builder surface: every configuration method returns a transport that
/// still produces a working router, and the session handle it hands back
/// reports the (empty) session store.
#[tokio::test]
async fn builder_configuration_produces_a_usable_router() {
    use std::sync::Arc;

    use tower::timeout::TimeoutLayer;
    use tower_mcp::event_store::MemoryEventStore;
    use tower_mcp::session_store::MemorySessionStore;
    use tower_mcp::transport::http::SessionConfig;

    let transport = UnixSocketTransport::new(test_router())
        .with_sampling()
        .session_config(SessionConfig::default())
        .session_ttl(Duration::from_secs(60))
        .max_sessions(4)
        .session_store(Arc::new(MemorySessionStore::new()))
        .event_store(Arc::new(MemoryEventStore::with_capacity(16)))
        .auto_reinitialize_sessions(true)
        .disable_origin_validation()
        .allowed_origins(vec!["http://localhost".to_string()])
        .disable_host_validation()
        .allowed_hosts(vec!["localhost".to_string()])
        .cleanup_on_bind(true)
        .layer(TimeoutLayer::new(Duration::from_secs(30)));

    let (_router, handle) = transport.into_router_with_handle();
    assert_eq!(handle.session_count().await, 0);
    assert!(handle.list_sessions().await.is_empty());
}

/// The gap #1285 was filed for: `serve` owns the accept loop and never
/// returns, so a process that starts one cannot stop it without exiting.
///
/// A hang is the failure this guards against, so every wait here is bounded
/// and the test fails on the timeout rather than blocking the suite.
#[tokio::test]
async fn serve_with_shutdown_returns_and_stops_accepting() {
    let socket = SocketPath::new("shutdown");
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let serve_path = socket.path().to_path_buf();

    let server = tokio::spawn(
        UnixSocketTransport::new(test_router())
            .disable_origin_validation()
            .serve_with_shutdown(serve_path, async move {
                stop_rx.await.ok();
            }),
    );

    // The server is genuinely up: a real MCP round trip is answered first, so
    // what stops below is a working server rather than a failed bind.
    wait_until_connectable(socket.path()).await;
    let init = post_json(socket.path(), &[], &initialize_request()).await;
    assert_eq!(init["result"]["serverInfo"]["name"], "unix-socket-test");

    stop_tx.send(()).expect("send shutdown signal");

    let served = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("serve_with_shutdown never returned after the signal")
        .expect("server task panicked");
    served.expect("serve_with_shutdown reported an error");

    // Returning is not the whole claim: the listener has to be gone too, or
    // the caller has stopped waiting on a server that is still accepting.
    assert!(
        UnixStream::connect(socket.path()).await.is_err(),
        "socket still accepted a connection after shutdown"
    );
}

/// `drain_timeout` bounds the wait for connections that are still open.
///
/// The parked handler never finishes, so without the bound this test would
/// hang: that is exactly the case the bound exists for. The other half of
/// the pair, that an unbounded shutdown really does wait for an in-flight
/// request rather than returning early, is in `graceful_shutdown.rs`.
#[tokio::test]
async fn drain_timeout_returns_while_a_request_is_still_in_flight() {
    let socket = SocketPath::new("drain");
    let park = ParkedTool::new();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let serve_path = socket.path().to_path_buf();

    let server = tokio::spawn(
        UnixSocketTransport::new(park.router())
            .disable_origin_validation()
            .drain_timeout(Duration::from_millis(200))
            .serve_with_shutdown(serve_path, async move {
                stop_rx.await.ok();
            }),
    );

    wait_until_connectable(socket.path()).await;

    let call_path = socket.path().to_path_buf();
    let call = tokio::spawn(async move {
        post(
            &call_path,
            &[("MCP-Protocol-Version", PROTOCOL_VERSION)],
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "park", "arguments": {}}
            })
            .to_string(),
        )
        .await
    });

    park.started().await;
    stop_tx.send(()).expect("send shutdown signal");

    let served = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("drain_timeout did not bound the wait for an open connection")
        .expect("server task panicked");
    served.expect("serve_with_shutdown reported an error");

    // The bound is what ended the wait: the request it was waiting on is
    // still unanswered.
    assert!(
        !call.is_finished(),
        "the parked request completed, so the drain was never actually blocked"
    );

    call.abort();
    park.release();
}

/// A tool that does not return until the test releases it, so a request can
/// be held in flight across a shutdown.
struct ParkedTool {
    started: std::sync::Arc<tokio::sync::Notify>,
    release: std::sync::Arc<tokio::sync::Notify>,
}

impl ParkedTool {
    fn new() -> Self {
        Self {
            started: std::sync::Arc::new(tokio::sync::Notify::new()),
            release: std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn router(&self) -> McpRouter {
        let started = self.started.clone();
        let release = self.release.clone();
        let park = ToolBuilder::new("park")
            .description("Blocks until the test releases it")
            .handler(move |_input: serde_json::Value| {
                let started = started.clone();
                let release = release.clone();
                async move {
                    // notify_one stores a permit, so this cannot race the
                    // test's own await.
                    started.notify_one();
                    release.notified().await;
                    Ok(CallToolResult::text("released"))
                }
            })
            .build();

        McpRouter::new()
            .server_info("unix-socket-test", "1.0.0")
            .tool(park)
    }

    /// Resolves once the handler is running.
    async fn started(&self) {
        self.started.notified().await;
    }

    fn release(&self) {
        self.release.notify_one();
    }
}

/// `from_service` accepts a pre-built service, the same as `HttpTransport`.
#[tokio::test]
async fn from_service_serves_the_wrapped_service() {
    let socket = SocketPath::new("service");
    let transport = UnixSocketTransport::from_service(test_router()).disable_origin_validation();
    serve_in_background(transport, socket.path()).await;

    let init = post_json(socket.path(), &[], &initialize_request()).await;
    assert_eq!(init["result"]["serverInfo"]["name"], "unix-socket-test");
}
