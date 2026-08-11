//! Graceful shutdown for the transports that own their listener (#1285).
//!
//! `UnixSocketTransport` is covered in `unix_socket.rs`, alongside the rest
//! of its coverage. Here are the HTTP and WebSocket halves, plus both sides
//! of the drain contract the shared implementation has to get right: an
//! unbounded shutdown answers what is already in flight, and a bounded one
//! stops waiting for it.
//!
//! Every wait in this file is bounded. The failure being guarded against is
//! a future that never resolves, so a hang has to surface as a failed
//! assertion rather than a stuck test run.

#![cfg(any(feature = "http", feature = "websocket"))]

use std::time::Duration;

/// An ephemeral port, released before the transport binds it.
///
/// `serve_with_shutdown` binds the address itself and does not report which
/// one it got, so the port has to be picked before the server starts.
async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

#[cfg(feature = "http")]
mod http {
    use super::*;

    use std::sync::Arc;

    use tokio::sync::{Notify, oneshot};
    use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

    const PROTOCOL_VERSION: &str = "2025-11-25";

    /// A tool that does not return until the test releases it, so a request
    /// can be held in flight across a shutdown.
    struct ParkedTool {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl ParkedTool {
        fn new() -> Self {
            Self {
                started: Arc::new(Notify::new()),
                release: Arc::new(Notify::new()),
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
                        // notify_one stores a permit, so this cannot race
                        // the test's own await.
                        started.notify_one();
                        release.notified().await;
                        Ok(CallToolResult::text("released"))
                    }
                })
                .build();

            McpRouter::new()
                .server_info("http-shutdown-test", "1.0.0")
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

    /// Block until the server answers its health probe, so no test races the
    /// bind.
    async fn wait_until_serving(port: u16) {
        let health = format!("http://127.0.0.1:{port}/health");
        let client = reqwest::Client::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if let Ok(response) = client.get(&health).send().await
                && response.status().is_success()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("server on port {port} never started serving");
    }

    /// Call the parked tool. Resolves only once the handler is released.
    async fn call_park(port: u16) -> serde_json::Value {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/"))
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "park", "arguments": {}}
            }))
            .send()
            .await
            .expect("send tools/call")
            .json()
            .await
            .expect("decode tools/call response")
    }

    /// The unbounded default. The signal closes the listener but does not
    /// abandon a request that is already running: it is answered, and only
    /// then does `serve_with_shutdown` return.
    ///
    /// This is the other half of `drain_timeout`'s pair. It is what makes
    /// the bounded test meaningful, because it shows the drain really does
    /// block on an in-flight request.
    #[tokio::test]
    async fn shutdown_waits_for_a_request_that_is_already_running() {
        let park = ParkedTool::new();
        let port = free_port().await;
        let (stop_tx, stop_rx) = oneshot::channel::<()>();

        let router = park.router();
        let mut server = tokio::spawn(async move {
            HttpTransport::new(router)
                .serve_with_shutdown(&format!("127.0.0.1:{port}"), async move {
                    stop_rx.await.ok();
                })
                .await
        });

        wait_until_serving(port).await;
        let call = tokio::spawn(async move { call_park(port).await });

        park.started().await;
        stop_tx.send(()).expect("send shutdown signal");

        // The signal alone must not end the request the server accepted.
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut server)
                .await
                .is_err(),
            "serve_with_shutdown returned while a request was still running"
        );

        park.release();

        let served = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("serve_with_shutdown never returned after the request finished")
            .expect("server task panicked");
        served.expect("serve_with_shutdown reported an error");

        let body = call.await.expect("client task panicked");
        assert_eq!(
            body["result"]["content"][0]["text"], "released",
            "the in-flight request was dropped instead of answered: {body}"
        );
    }

    /// `drain_timeout` bounds that wait. The parked handler is never
    /// released, so without the bound this test would hang.
    #[tokio::test]
    async fn drain_timeout_returns_while_a_request_is_still_in_flight() {
        let park = ParkedTool::new();
        let port = free_port().await;
        let (stop_tx, stop_rx) = oneshot::channel::<()>();

        let router = park.router();
        let server = tokio::spawn(async move {
            HttpTransport::new(router)
                .drain_timeout(Duration::from_millis(200))
                .serve_with_shutdown(&format!("127.0.0.1:{port}"), async move {
                    stop_rx.await.ok();
                })
                .await
        });

        wait_until_serving(port).await;
        let call = tokio::spawn(async move { call_park(port).await });

        park.started().await;
        stop_tx.send(()).expect("send shutdown signal");

        let served = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("drain_timeout did not bound the wait for an in-flight request")
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
}

#[cfg(feature = "websocket")]
mod websocket {
    use super::*;

    use tokio::sync::oneshot;
    use tower_mcp::client::{McpClient, WebSocketClientTransport};
    use tower_mcp::{CallToolResult, McpRouter, ToolBuilder, WebSocketTransport};

    fn router() -> McpRouter {
        let echo = ToolBuilder::new("echo")
            .description("Echo a value")
            .handler(|v: serde_json::Value| async move { Ok(CallToolResult::text(v.to_string())) })
            .build();

        McpRouter::new()
            .server_info("ws-shutdown-test", "1.0.0")
            .tool(echo)
    }

    async fn connect(url: &str) -> WebSocketClientTransport {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match WebSocketClientTransport::connect(url).await {
                Ok(transport) => return transport,
                Err(e) if tokio::time::Instant::now() >= deadline => {
                    panic!("never connected to {url}: {e}")
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    }

    /// Shutting down with a client still connected returns, and stops new
    /// clients getting in.
    ///
    /// It returns without waiting for that client, which is why this
    /// transport has no `drain_timeout`: an upgraded WebSocket has already
    /// left the connection axum tracks, so there is no drain for a bound to
    /// cut short. The socket itself stays up, on its own task, until its
    /// client hangs up or the process exits.
    #[tokio::test]
    async fn shutdown_returns_and_stops_accepting_with_a_client_connected() {
        let port = free_port().await;
        let (stop_tx, stop_rx) = oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            WebSocketTransport::new(router())
                .serve_with_shutdown(&format!("127.0.0.1:{port}"), async move {
                    stop_rx.await.ok();
                })
                .await
        });

        let url = format!("ws://127.0.0.1:{port}/");
        let client = McpClient::connect(connect(&url).await)
            .await
            .expect("build client");
        let initialized = client
            .initialize("shutdown-test", "1.0.0")
            .await
            .expect("initialize");
        assert_eq!(initialized.server_info.name, "ws-shutdown-test");

        stop_tx.send(()).expect("send shutdown signal");

        let served = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("serve_with_shutdown never returned after the signal")
            .expect("server task panicked");
        served.expect("serve_with_shutdown reported an error");

        // The client held its socket for the whole shutdown, which is what
        // makes this more than the no-clients case.
        drop(client);

        // Returning is not the whole claim: the listener has to be gone too.
        assert!(
            WebSocketClientTransport::connect(&url).await.is_err(),
            "the port still accepted a connection after shutdown"
        );
    }
}
