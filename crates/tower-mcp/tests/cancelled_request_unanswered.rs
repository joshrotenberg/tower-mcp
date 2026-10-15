//! A request the client cancels with `notifications/cancelled` is not answered
//! (#1547).
//!
//! MCP cancellation utility, Behavior Requirements: a receiver SHOULD NOT send
//! a response for a request it was asked to cancel. Before this, a handler that
//! observed `ctx.cancelled()` and returned had its result written anyway.
//!
//! Every stdio transport shape is driven over an in-memory duplex: the plain
//! transport, a layered (generic) one, and the bidirectional one. Each wait is
//! bounded so a regression fails rather than hangs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::time::timeout;
use tower_mcp::extract::{Context, RawArgs, State};
use tower_mcp::transport::stdio::BidirectionalStdioTransport;
use tower_mcp::{CallToolResult, McpRouter, StdioTransport, ToolBuilder};

const STEP: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
enum Shape {
    Plain,
    Layered,
    Bidirectional,
}

const SHAPES: [Shape; 3] = [Shape::Plain, Shape::Layered, Shape::Bidirectional];

/// Set by the `wait` tool once it has seen the cancellation and is returning.
#[derive(Clone, Default)]
struct Finished(Arc<AtomicBool>);

fn router(finished: Finished) -> McpRouter {
    let wait = ToolBuilder::new("wait")
        .description("Waits for cancellation, then returns a result anyway")
        .extractor_handler(
            finished,
            |State(finished): State<Finished>, ctx: Context, RawArgs(_): RawArgs| async move {
                ctx.cancelled().await;
                finished.0.store(true, Ordering::SeqCst);
                Ok(CallToolResult::text("stopped after cancellation"))
            },
        )
        .build();
    let echo = ToolBuilder::new("echo")
        .description("Answers immediately")
        .extractor_handler((), |RawArgs(_): RawArgs| async move {
            Ok(CallToolResult::text("echoed"))
        })
        .build();
    McpRouter::new()
        .server_info("cancel-test", "0.0.0")
        .tool(wait)
        .tool(echo)
}

struct Client {
    writer: DuplexStream,
    reader: BufReader<DuplexStream>,
}

impl Client {
    /// Start `shape` serving `router`, then complete the handshake.
    async fn start(shape: Shape, router: McpRouter) -> Self {
        let (writer, server_stdin) = tokio::io::duplex(16 * 1024);
        let (server_stdout, reader) = tokio::io::duplex(16 * 1024);

        match shape {
            Shape::Plain => {
                let mut transport = StdioTransport::new(router);
                tokio::spawn(async move {
                    let _ = transport
                        .run_with_streams(server_stdin, server_stdout)
                        .await;
                });
            }
            Shape::Layered => {
                let mut transport = StdioTransport::new(router)
                    .layer(tower::timeout::TimeoutLayer::new(Duration::from_secs(60)));
                tokio::spawn(async move {
                    let _ = transport
                        .run_with_streams(server_stdin, server_stdout)
                        .await;
                });
            }
            Shape::Bidirectional => {
                let mut transport = BidirectionalStdioTransport::new(router);
                tokio::spawn(async move {
                    let _ = transport
                        .run_with_streams(server_stdin, server_stdout)
                        .await;
                });
            }
        }

        let mut client = Self {
            writer,
            reader: BufReader::new(reader),
        };
        client
            .send(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "t", "version": "0"}
                }
            }))
            .await;
        let init = client.next_frame().await.expect("initialize response");
        assert_eq!(init["id"], 1, "{shape:?}: {init}");
        client
            .send(serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .await;
        client
    }

    async fn send(&mut self, value: serde_json::Value) {
        self.writer
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn call(&mut self, id: i64, tool: &str) {
        self.send(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": {}}
        }))
        .await;
    }

    async fn cancel(&mut self, id: i64) {
        self.send(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": id, "reason": "test"}
        }))
        .await;
    }

    /// The next frame, or `None` if nothing arrives within `limit`.
    async fn next_frame_within(&mut self, limit: Duration) -> Option<serde_json::Value> {
        let mut line = String::new();
        let read = timeout(limit, self.reader.read_line(&mut line))
            .await
            .ok()?;
        assert!(read.expect("read from server output") > 0, "unexpected EOF");
        Some(serde_json::from_str(line.trim()).expect("a JSON frame"))
    }

    async fn next_frame(&mut self) -> Option<serde_json::Value> {
        self.next_frame_within(STEP).await
    }

    /// Collect frames until the response for `id` arrives.
    async fn frames_until_response(&mut self, id: i64) -> Vec<serde_json::Value> {
        let mut seen = Vec::new();
        loop {
            let frame = self
                .next_frame()
                .await
                .unwrap_or_else(|| panic!("no response for id {id}; saw {seen:?}"));
            let done = frame["id"] == id;
            seen.push(frame);
            if done {
                return seen;
            }
        }
    }
}

async fn wait_until(flag: &Arc<AtomicBool>) {
    timeout(STEP, async {
        while !flag.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the handler never observed the cancellation");
}

/// The repro from the issue: the handler returns after seeing the
/// cancellation, and nothing carrying its id is written.
#[tokio::test]
async fn a_cancelled_request_is_not_answered() {
    for shape in SHAPES {
        let finished = Finished::default();
        let mut client = Client::start(shape, router(finished.clone())).await;

        client.call(2, "wait").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        client.cancel(2).await;

        // The handler has returned, so a response would be on its way.
        wait_until(&finished.0).await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        // A later request round-trips; anything written for id 2 in the
        // meantime would be ahead of it or alongside it.
        client.call(3, "echo").await;
        let seen = client.frames_until_response(3).await;
        assert!(
            seen.iter().all(|frame| frame["id"] != 2),
            "{shape:?}: a cancelled request was answered: {seen:?}"
        );
        assert!(
            client
                .next_frame_within(Duration::from_secs(1))
                .await
                .is_none(),
            "{shape:?}: a frame arrived after the cancelled request's handler returned"
        );
    }
}

/// A request that is never cancelled is answered as before, including while a
/// sibling request is cancelled.
#[tokio::test]
async fn an_uncancelled_request_is_still_answered() {
    for shape in SHAPES {
        let finished = Finished::default();
        let mut client = Client::start(shape, router(finished.clone())).await;

        client.call(2, "echo").await;
        let seen = client.frames_until_response(2).await;
        assert_eq!(
            seen.last().unwrap()["result"]["content"][0]["text"],
            "echoed",
            "{shape:?}"
        );

        // Cancelling one request leaves a concurrent one alone.
        client.call(3, "wait").await;
        client.call(4, "echo").await;
        let seen = client.frames_until_response(4).await;
        assert_eq!(
            seen.last().unwrap()["result"]["content"][0]["text"],
            "echoed",
            "{shape:?}"
        );
        client.cancel(3).await;
        wait_until(&finished.0).await;
        client.call(5, "echo").await;
        let seen = client.frames_until_response(5).await;
        assert!(
            seen.iter().all(|frame| frame["id"] != 3),
            "{shape:?}: {seen:?}"
        );
    }
}

/// A cancellation for an id that is not in flight, or no longer is, changes
/// nothing: it does not suppress a request that arrives later under that id.
#[tokio::test]
async fn a_cancellation_for_an_idle_id_does_not_affect_a_later_request() {
    for shape in SHAPES {
        let mut client = Client::start(shape, router(Finished::default())).await;

        // Never seen.
        client.cancel(99).await;
        client.call(99, "echo").await;
        let seen = client.frames_until_response(99).await;
        assert_eq!(
            seen.last().unwrap()["result"]["content"][0]["text"],
            "echoed",
            "{shape:?}"
        );

        // Already finished, then the id is reused.
        client.cancel(99).await;
        client.call(99, "echo").await;
        let seen = client.frames_until_response(99).await;
        assert_eq!(
            seen.last().unwrap()["result"]["content"][0]["text"],
            "echoed",
            "{shape:?}"
        );
    }
}
