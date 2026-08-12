//! WebSocket equivalents of the malformed/stray-frame coverage in
//! `adversarial_input.rs` (#1335).
//!
//! `websocket.rs` has two receive loops -- `process_message` (no sampling)
//! and `handle_incoming_message` (`.with_sampling()`) -- and only one of them
//! got the #1272 conformance fix. Every test here drives a real
//! [`WebSocketTransport`] over an actual socket via a raw `tokio-tungstenite`
//! client, so it is not fooled by any validation the crate's own
//! [`tower_mcp::client::WebSocketClientTransport`] might add on the way out.
//! Each test says which loop it targets.

#![cfg(feature = "websocket")]

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::tungstenite::protocol::Message;
use tower_mcp::{McpRouter, WebSocketTransport};

// ============================================================================
// Harness
// ============================================================================

type RawWs = tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// A running [`WebSocketTransport`] plus a raw client socket, bypassing this
/// crate's own client transport so nothing sanitizes a frame before it hits
/// the wire.
struct Server {
    ws: RawWs,
}

impl Server {
    /// Start a server in simple (no-sampling) mode, driving `process_message`.
    async fn simple(router: McpRouter) -> Self {
        Self::start(router, false).await
    }

    /// Start a server with sampling enabled, driving `handle_incoming_message`.
    async fn bidirectional(router: McpRouter) -> Self {
        Self::start(router, true).await
    }

    async fn start(router: McpRouter, sampling: bool) -> Self {
        let mut transport = WebSocketTransport::new(router);
        if sampling {
            transport = transport.with_sampling();
        }
        let app = transport.into_router();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        // Let the listener come up before the first dial.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let (ws, _response) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .expect("connect");
        Self { ws }
    }

    /// Send one raw text frame, exactly as given.
    async fn send_raw(&mut self, text: &str) {
        self.ws
            .send(Message::Text(text.to_string().into()))
            .await
            .unwrap();
    }

    async fn send(&mut self, frame: serde_json::Value) {
        self.send_raw(&frame.to_string()).await;
    }

    /// Read the next text frame, failing the test rather than hanging.
    async fn next_frame(&mut self) -> serde_json::Value {
        self.next_frame_within(Duration::from_secs(5))
            .await
            .expect("no frame arrived before the deadline")
    }

    async fn next_frame_within(&mut self, budget: Duration) -> Option<serde_json::Value> {
        let read =
            async {
                loop {
                    match self.ws.next().await {
                        Some(Ok(Message::Text(text))) => {
                            return Some(serde_json::from_str(&text).unwrap_or_else(|e| {
                                panic!("server wrote invalid JSON: {e}: {text}")
                            }));
                        }
                        Some(Ok(Message::Close(_))) => return None,
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => panic!("websocket error: {e}"),
                        None => return None,
                    }
                }
            };
        tokio::time::timeout(budget, read).await.ok().flatten()
    }

    /// Assert nothing more arrives within `budget`.
    async fn expect_silence(&mut self, budget: Duration) {
        if let Some(frame) = self.next_frame_within(budget).await {
            panic!("expected no further frames, got: {frame}");
        }
    }

    async fn initialize(&mut self) {
        self.initialize_with_version("2025-11-25").await;
    }

    /// Negotiate a specific protocol revision, needed for the batch tests: a
    /// top-level JSON-RPC batch is only accepted by MCP 2025-03-26.
    async fn initialize_with_version(&mut self, version: &str) {
        self.send(serde_json::json!({
            "jsonrpc": "2.0", "id": "init", "method": "initialize",
            "params": {
                "protocolVersion": version,
                "capabilities": {},
                "clientInfo": {"name": "adversary", "version": "1.0.0"}
            }
        }))
        .await;
        let init = self.next_frame().await;
        assert_eq!(init["id"], "init", "unexpected initialize reply: {init}");
        self.send(serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }))
        .await;
    }
}

fn ping(id: i64) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "ping"})
}

fn base_router() -> McpRouter {
    McpRouter::new().server_info("adversarial-ws", "1.0.0")
}

// ============================================================================
// 1. `handle_incoming_message` (bidirectional / `.with_sampling()`)
// ============================================================================

/// Control: general MCP validation already refuses a request whose `method`
/// is the wrong JSON type, before the untagged `JsonRpcMessage` parse is ever
/// reached. Kept as a guard so the rewrite in #1335 does not regress it.
#[tokio::test]
async fn a_request_with_a_wrong_typed_method_is_refused() {
    let mut server = Server::bidirectional(base_router()).await;
    server.initialize().await;

    server
        .send_raw(r#"{"jsonrpc":"2.0","id":1,"method":42}"#)
        .await;
    let frame = server.next_frame_within(Duration::from_secs(2)).await;
    let frame =
        frame.unwrap_or_else(|| panic!("a request with a wrong-typed method must be answered"));
    assert!(
        frame.get("error").is_some(),
        "a malformed request must be refused, not accepted: {frame}"
    );

    // The connection must keep serving afterward.
    server.send(ping(2)).await;
    let pong = server.next_frame().await;
    assert_eq!(pong["id"], 2);
    assert!(pong.get("result").is_some(), "ping must work: {pong}");
}

/// A batch may mix requests and notifications (general MCP validation allows
/// this), but `JsonRpcMessage::Batch` requires every member to deserialize as
/// a full `JsonRpcRequest`, which a notification-shaped member (no `id`)
/// cannot. That failure reaches the untagged parse in `handle_incoming_message`
/// after validation has already passed it, and before #1335 the `?` on that
/// parse propagates to a caller that only logs -- the client hangs forever
/// instead of getting a `-32700`.
#[tokio::test]
async fn a_batch_that_fails_the_untagged_parse_after_validation_still_answers() {
    let mut server = Server::bidirectional(base_router()).await;
    server.initialize_with_version("2025-03-26").await;

    server
        .send_raw(
            r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/bogus"}]"#,
        )
        .await;
    let frame = server.next_frame_within(Duration::from_secs(2)).await;
    let frame = frame.unwrap_or_else(|| {
        panic!(
            "a batch that fails the untagged parse after validation must still be answered, not ignored"
        )
    });
    assert!(
        frame.get("error").is_some(),
        "must be refused, not silently accepted: {frame}"
    );

    // The connection must keep serving afterward.
    server.send(ping(2)).await;
    let pong = server.next_frame().await;
    assert_eq!(pong["id"], 2);
    assert!(pong.get("result").is_some(), "ping must work: {pong}");
}

/// The classification order must match stdio: classify before validating. A
/// response frame whose id does not fit `i64` fails general MCP validation,
/// but it is still shaped like a response (an id, no method, a `result`) and
/// must be silently ignored rather than answered with a validation error
/// that leaks internal detail about a frame the client never asked about.
#[tokio::test]
async fn a_response_shaped_frame_that_fails_validation_is_still_ignored() {
    let mut server = Server::bidirectional(base_router()).await;
    server.initialize().await;

    server
        .send_raw(r#"{"jsonrpc":"2.0","id":99999999999999999999,"result":{}}"#)
        .await;
    server.expect_silence(Duration::from_millis(300)).await;

    // The connection must keep serving afterward.
    server.send(ping(2)).await;
    let pong = server.next_frame().await;
    assert_eq!(pong["id"], 2);
}

/// Control: a genuine stray response frame over the bidirectional loop
/// (already handled correctly before #1335, but worth pinning since the
/// fix rewrites the surrounding function).
#[tokio::test]
async fn a_stray_response_frame_is_not_answered_bidirectional() {
    let mut server = Server::bidirectional(base_router()).await;
    server.initialize().await;

    server
        .send_raw(r#"{"jsonrpc":"2.0","id":4242,"result":{}}"#)
        .await;
    server.expect_silence(Duration::from_millis(300)).await;

    server.send(ping(2)).await;
    assert_eq!(server.next_frame().await["id"], 2);
}

/// A BOM-prefixed frame is accepted over the bidirectional loop, the way the
/// crate's own websocket client already strips one on receive (#1303,
/// `client/websocket.rs`), and the way stdio does on the server side.
#[tokio::test]
async fn a_bom_prefixed_frame_is_accepted_bidirectional() {
    let mut server = Server::bidirectional(base_router()).await;
    server.initialize().await;

    server.send_raw(&format!("\u{feff}{}", ping(1))).await;
    let frame = server.next_frame().await;
    assert_eq!(frame["id"], 1);
    assert!(
        frame.get("result").is_some(),
        "BOM must be stripped: {frame}"
    );
}

// ============================================================================
// 2. `process_message` (simple / unidirectional)
// ============================================================================

/// A stray response frame arriving on the unidirectional loop must be
/// ignored, not answered with a `-32700` that names an internal Rust type.
/// This transport never sends requests, so a response is always the peer's
/// mistake -- the same reasoning stdio's `process_line` documents.
#[tokio::test]
async fn a_stray_response_frame_is_not_answered_simple() {
    let mut server = Server::simple(base_router()).await;
    server.initialize().await;

    server
        .send_raw(r#"{"jsonrpc":"2.0","id":4242,"result":{}}"#)
        .await;
    server.expect_silence(Duration::from_millis(300)).await;

    server.send(ping(2)).await;
    assert_eq!(server.next_frame().await["id"], 2);
}

/// A BOM-prefixed frame is accepted over the unidirectional loop too.
#[tokio::test]
async fn a_bom_prefixed_frame_is_accepted_simple() {
    let mut server = Server::simple(base_router()).await;
    server.initialize().await;

    server.send_raw(&format!("\u{feff}{}", ping(1))).await;
    let frame = server.next_frame().await;
    assert_eq!(frame["id"], 1);
    assert!(
        frame.get("result").is_some(),
        "BOM must be stripped: {frame}"
    );
}
