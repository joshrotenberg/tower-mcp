//! rmcp #967 analog: on the 2026-07-28 sessionless POST path, a client
//! disconnect cancels the in-flight request's `CancellationToken`, so
//! handler-spawned work observes the disconnect instead of running to
//! completion for a client that is gone.

#![cfg(all(feature = "http", feature = "stateless"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tower_mcp::extract::{Context, RawArgs, State};
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

/// Shared flags the handler reports through.
#[derive(Clone, Default)]
struct Flags {
    /// Set by the handler as soon as it starts running.
    started: Arc<AtomicBool>,
    /// Set by a handler-spawned watcher when the request token cancels.
    cancelled: Arc<AtomicBool>,
}

fn router(flags: Flags) -> McpRouter {
    let tool = ToolBuilder::new("slow")
        .description("Sleeps forever; reports cancellation via shared flags.")
        .extractor_handler(
            flags,
            |State(flags): State<Flags>, ctx: Context, RawArgs(_): RawArgs| async move {
                flags.started.store(true, Ordering::SeqCst);

                // Spawned work holding a token clone observes the
                // disconnect even after the request future is dropped.
                let token = ctx.cancellation_token();
                let cancelled = flags.cancelled.clone();
                tokio::spawn(async move {
                    token.cancelled().await;
                    cancelled.store(true, Ordering::SeqCst);
                });

                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(CallToolResult::text("done"))
            },
        )
        .build();
    McpRouter::new()
        .server_info("disconnect-test", "1.0.0")
        .tool(tool)
}

async fn wait_for(flag: &AtomicBool, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    flag.load(Ordering::SeqCst)
}

/// A raw HTTP/1.1 POST for a stateless (2026-07-28) tools/call, written
/// over a plain TCP stream so the test controls the connection lifetime.
fn stateless_call_request(body: &str) -> String {
    format!(
        "POST / HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Content-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\n\
         MCP-Protocol-Version: 2026-07-28\r\n\
         Mcp-Method: tools/call\r\n\
         Mcp-Name: slow\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {}",
        body.len(),
        body
    )
}

#[tokio::test]
async fn stateless_client_disconnect_cancels_in_flight_handler() {
    let flags = Flags::default();
    let app = HttpTransport::new(router(flags.clone()))
        .disable_origin_validation()
        .into_router();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "slow", "arguments": {}}
    })
    .to_string();

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(stateless_call_request(&body).as_bytes())
        .await
        .unwrap();
    stream.flush().await.unwrap();

    // The handler must be running before the disconnect for the test to
    // prove in-flight cancellation.
    assert!(
        wait_for(&flags.started, Duration::from_secs(5)).await,
        "handler never started"
    );
    assert!(
        !flags.cancelled.load(Ordering::SeqCst),
        "token must not be cancelled while the client is connected"
    );

    // Disconnect mid-call.
    drop(stream);

    assert!(
        wait_for(&flags.cancelled, Duration::from_secs(5)).await,
        "handler did not observe cancellation after client disconnect"
    );
}

#[tokio::test]
async fn stateless_normal_completion_does_not_cancel() {
    let flags = Flags::default();
    let fast = {
        let flags = flags.clone();
        ToolBuilder::new("fast")
            .description("Returns immediately; watcher records cancellation.")
            .extractor_handler(
                flags,
                |State(flags): State<Flags>, ctx: Context, RawArgs(_): RawArgs| async move {
                    let token = ctx.cancellation_token();
                    let cancelled = flags.cancelled.clone();
                    tokio::spawn(async move {
                        token.cancelled().await;
                        cancelled.store(true, Ordering::SeqCst);
                    });
                    Ok(CallToolResult::text("done"))
                },
            )
            .build()
    };
    let router = McpRouter::new()
        .server_info("disconnect-test", "1.0.0")
        .tool(fast);
    let app = HttpTransport::new(router)
        .disable_origin_validation()
        .into_router();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "fast", "arguments": {}}
    })
    .to_string();

    let request = format!(
        "POST / HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Content-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\n\
         MCP-Protocol-Version: 2026-07-28\r\n\
         Mcp-Method: tools/call\r\n\
         Mcp-Name: fast\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {}",
        body.len(),
        body
    );

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    // Read the response so the request completes while connected.
    let mut buf = vec![0u8; 4096];
    use tokio::io::AsyncReadExt;
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .expect("timed out reading response")
        .unwrap();
    let response = String::from_utf8_lossy(&buf[..n]);
    assert!(response.starts_with("HTTP/1.1 200"), "got: {}", response);

    // The guard was disarmed on response production; closing the
    // connection afterwards must not signal cancellation.
    drop(stream);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !flags.cancelled.load(Ordering::SeqCst),
        "normal completion must not cancel the request token"
    );
}
