//! Adversarial input coverage for request handling (#1258, axis 4).
//!
//! Everything here drives a real [`StdioTransport`] over in-memory duplex
//! streams, so the frames are exactly what a hostile or buggy peer would put
//! on the wire: duplicate ids, stale notifications, malformed envelopes,
//! out-of-order lifecycle traffic, and handlers that misbehave.
//!
//! Three tests are `#[ignore]`d. Each one asserts the behaviour the code should
//! have and fails against the behaviour it has today; the attribute carries
//! the reason. Run them with `cargo test --test adversarial_input -- --ignored`
//! to reproduce every finding.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::task::JoinHandle;
use tower_mcp::extract::{Context, RawArgs};
use tower_mcp::{CallToolResult, McpRouter, StdioTransport, ToolBuilder};

// ============================================================================
// Harness
// ============================================================================

/// A running [`StdioTransport`] plus both ends of its byte streams.
struct Server {
    writer: DuplexStream,
    reader: BufReader<DuplexStream>,
    task: JoinHandle<tower_mcp::Result<()>>,
}

impl Server {
    fn start(mut transport: StdioTransport) -> Self {
        // 8 MiB so the oversized-frame probes are bounded by the code under
        // test rather than by the pipe.
        let (writer, server_stdin) = tokio::io::duplex(8 * 1024 * 1024);
        let (server_stdout, reader) = tokio::io::duplex(8 * 1024 * 1024);
        let task = tokio::spawn(async move {
            transport
                .run_with_streams(server_stdin, server_stdout)
                .await
        });
        Self {
            writer,
            reader: BufReader::new(reader),
            task,
        }
    }

    fn with_router(router: McpRouter) -> Self {
        Self::start(StdioTransport::new(router))
    }

    /// Write one raw line, exactly as given, plus the newline delimiter.
    async fn send_raw(&mut self, line: &str) {
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.write_all(b"\n").await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn send(&mut self, frame: serde_json::Value) {
        self.send_raw(&frame.to_string()).await;
    }

    /// Read the next frame, failing the test rather than hanging.
    async fn next_frame(&mut self) -> serde_json::Value {
        self.next_frame_within(Duration::from_secs(5))
            .await
            .expect("no frame arrived before the deadline")
    }

    async fn next_frame_within(&mut self, budget: Duration) -> Option<serde_json::Value> {
        let read = async {
            loop {
                let mut line = String::new();
                let n = self.reader.read_line(&mut line).await.expect("read stdout");
                if n == 0 {
                    return None;
                }
                if line.trim().is_empty() {
                    continue;
                }
                return Some(
                    serde_json::from_str(line.trim())
                        .unwrap_or_else(|e| panic!("server wrote invalid JSON: {e}: {line}")),
                );
            }
        };
        tokio::time::timeout(budget, read).await.ok().flatten()
    }

    /// Collect `n` frames. Requests run concurrently, so this is completion
    /// order, not send order; callers pair by id.
    async fn next_frames(&mut self, n: usize) -> Vec<serde_json::Value> {
        let mut frames = Vec::with_capacity(n);
        for _ in 0..n {
            frames.push(self.next_frame().await);
        }
        frames
    }

    /// Assert nothing more arrives within `budget`.
    async fn expect_silence(&mut self, budget: Duration) {
        if let Some(frame) = self.next_frame_within(budget).await {
            panic!("expected no further frames, got: {frame}");
        }
    }

    async fn initialize(&mut self) {
        self.send(serde_json::json!({
            "jsonrpc": "2.0", "id": "init", "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
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

    /// Close stdin and wait for the loop to finish.
    async fn shutdown(self) {
        drop(self.writer);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
    }
}

fn call(id: serde_json::Value, name: &str, args: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": {"name": name, "arguments": args}
    })
}

fn cancel(id: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": {"requestId": id, "reason": "test"}
    })
}

fn ping(id: i64) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "ping"})
}

// ============================================================================
// Routers
// ============================================================================

/// A tool that polls its cancellation token and reports which way it ended.
///
/// `hold_ms` bounds the run so a test can never hang: an uncancelled call
/// answers "completed", a cancelled one answers "cancelled".
fn cancellable_tool(name: &'static str, hold_ms: u64) -> tower_mcp::Tool {
    ToolBuilder::new(name)
        .description("Runs until cancelled or until its budget expires")
        .extractor_handler((), move |ctx: Context, RawArgs(_): RawArgs| async move {
            let deadline = std::time::Instant::now() + Duration::from_millis(hold_ms);
            while std::time::Instant::now() < deadline {
                if ctx.is_cancelled() {
                    return Ok(CallToolResult::text("cancelled"));
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok(CallToolResult::text("completed"))
        })
        .build()
}

fn echo_tool() -> tower_mcp::Tool {
    ToolBuilder::new("echo")
        .description("Echoes its arguments back as text")
        .extractor_handler((), |RawArgs(args): RawArgs| async move {
            Ok(CallToolResult::text(args.to_string()))
        })
        .build()
}

fn base_router() -> McpRouter {
    McpRouter::new()
        .server_info("adversarial", "1.0.0")
        .tool(echo_tool())
        .tool(cancellable_tool("hold", 3_000))
        .tool(cancellable_tool("brief", 60))
}

// ============================================================================
// 1. Redundant and stale input
// ============================================================================

/// Control: a cancellation naming a request the server has never seen must be
/// ignored, and must not poison the id for a later request that reuses it.
///
/// This is also the ordering trap the axis calls out: a cancellation that
/// arrives before its request is a stale frame, not a standing instruction.
/// The sleep is what makes the test valid rather than racy -- without it the
/// notification and the request would be in flight together and a pass would
/// prove nothing.
#[tokio::test]
async fn cancellation_for_an_unseen_request_id_is_ignored() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server.send(cancel(serde_json::json!(1))).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    server
        .send(call(serde_json::json!(1), "brief", serde_json::json!({})))
        .await;

    let frame = server.next_frame().await;
    assert_eq!(frame["id"], 1);
    assert_eq!(
        frame["result"]["content"][0]["text"], "completed",
        "a stale cancellation must not cancel a later request reusing the id: {frame}"
    );
    server.shutdown().await;
}

/// Control: cancelling a request that already answered is a no-op, and the
/// connection keeps serving.
#[tokio::test]
async fn cancellation_after_completion_is_ignored() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server
        .send(call(serde_json::json!(1), "brief", serde_json::json!({})))
        .await;
    let done = server.next_frame().await;
    assert_eq!(done["result"]["content"][0]["text"], "completed");

    server.send(cancel(serde_json::json!(1))).await;
    server.send(ping(2)).await;
    let pong = server.next_frame().await;
    assert_eq!(pong["id"], 2);
    assert!(
        pong.get("result").is_some(),
        "the server must still serve: {pong}"
    );
    server.shutdown().await;
}

/// Control: request ids are typed, so a cancellation for the string `"7"`
/// must not reach the request whose id is the number `7`.
#[tokio::test]
async fn a_cancellation_with_a_mismatched_id_type_is_not_applied() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server
        .send(call(serde_json::json!(7), "brief", serde_json::json!({})))
        .await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    server.send(cancel(serde_json::json!("7"))).await;

    let frame = server.next_frame().await;
    assert_eq!(
        frame["result"]["content"][0]["text"], "completed",
        "a string id must not match a numeric one: {frame}"
    );
    server.shutdown().await;
}

/// Control, and the valid-construction proof for the two ignored tests below:
/// under distinct ids, two concurrent calls are answered independently and
/// cancelling one leaves the other running.
#[tokio::test]
async fn concurrent_calls_under_distinct_ids_are_independently_cancellable() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server
        .send(call(serde_json::json!(1), "hold", serde_json::json!({})))
        .await;
    server
        .send(call(serde_json::json!(2), "hold", serde_json::json!({})))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.send(cancel(serde_json::json!(1))).await;

    let frames = server.next_frames(2).await;
    let by_id = |id: i64| {
        frames
            .iter()
            .find(|f| f["id"] == serde_json::json!(id))
            .unwrap_or_else(|| panic!("no response for id {id} in {frames:#?}"))
    };
    assert_eq!(by_id(1)["result"]["content"][0]["text"], "cancelled");
    assert_eq!(by_id(2)["result"]["content"][0]["text"], "completed");
    server.shutdown().await;
}

/// Two in-flight requests share one id. Cancelling that id must reach every
/// request still running under it: the id is the only handle a client has.
///
/// Today `McpRouter` tracks in-flight work in a `HashMap<RequestId, _>`, so
/// the second registration evicts the first and only the last request to
/// arrive is reachable. The first runs to completion after its cancellation.
#[tokio::test]
#[ignore = "BUG: McpRouter::in_flight is keyed by RequestId alone, so a duplicate id evicts the first request's cancellation token"]
async fn cancelling_a_duplicated_id_reaches_every_request_using_it() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server
        .send(call(serde_json::json!(7), "hold", serde_json::json!({})))
        .await;
    server
        .send(call(serde_json::json!(7), "hold", serde_json::json!({})))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.send(cancel(serde_json::json!(7))).await;

    let frames = server.next_frames(2).await;
    let outcomes: Vec<&str> = frames
        .iter()
        .map(|f| f["result"]["content"][0]["text"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(
        outcomes,
        vec!["cancelled", "cancelled"],
        "both requests carrying the cancelled id must stop: {frames:#?}"
    );
    server.shutdown().await;
}

/// A short call and a long call share an id. When the short one answers, the
/// long one is still running under that id and must stay cancellable.
///
/// Today the short call's completion removes the map entry the long call
/// installed, so the later cancellation finds nothing to cancel.
#[tokio::test]
#[ignore = "BUG: McpRouter::complete_request removes the whole id entry, untracking a still-running request that shares the id"]
async fn completing_one_request_does_not_untrack_its_id_twin() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server
        .send(call(serde_json::json!(7), "hold", serde_json::json!({})))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    server
        .send(call(serde_json::json!(7), "brief", serde_json::json!({})))
        .await;

    let first = server.next_frame().await;
    assert_eq!(first["result"]["content"][0]["text"], "completed");

    server.send(cancel(serde_json::json!(7))).await;
    let second = server.next_frame().await;
    assert_eq!(
        second["result"]["content"][0]["text"], "cancelled",
        "the still-running request under id 7 must observe the cancellation: {second}"
    );
    server.shutdown().await;
}

/// Repeated identical requests must each be dispatched: the server may not
/// deduplicate work behind the client's back.
#[tokio::test]
async fn repeated_identical_requests_each_reach_the_handler() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let counted = ToolBuilder::new("count")
        .description("Counts invocations")
        .extractor_handler((), move |RawArgs(_): RawArgs| {
            let counter = counter.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(CallToolResult::text(n.to_string()))
            }
        })
        .build();
    let mut server = Server::with_router(
        McpRouter::new()
            .server_info("adversarial", "1.0.0")
            .tool(counted),
    );
    server.initialize().await;

    for id in 1..=3 {
        server
            .send(call(serde_json::json!(id), "count", serde_json::json!({})))
            .await;
    }
    let frames = server.next_frames(3).await;
    assert_eq!(frames.len(), 3);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "every call must run: {frames:#?}"
    );
    server.shutdown().await;
}

// ============================================================================
// 2. Malformed and hostile frames
// ============================================================================

/// A request whose id is `null` is invalid under MCP. It must be refused with
/// an invalid-request error rather than mistaken for a notification and
/// silently dropped, and the JSON parsed fine so it is not a parse error.
#[tokio::test]
async fn a_null_id_request_is_refused_not_swallowed() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server
        .send_raw(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#)
        .await;
    let frame = server.next_frame().await;
    assert!(
        frame.get("error").is_some(),
        "a null id must be refused: {frame}"
    );
    assert_eq!(
        frame["error"]["code"], -32600,
        "the JSON parsed cleanly, so this is an invalid request: {frame}"
    );
    server.shutdown().await;
}

/// Every structurally invalid envelope shape is refused with `-32600` and a
/// null id, and none of them takes the loop down.
#[tokio::test]
async fn structurally_invalid_envelopes_are_refused_uniformly() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    let cases: Vec<(&str, String)> = vec![
        ("empty batch", "[]".to_string()),
        (
            "fractional id",
            r#"{"jsonrpc":"2.0","id":1.5,"method":"ping"}"#.to_string(),
        ),
        (
            "object id",
            r#"{"jsonrpc":"2.0","id":{"a":1},"method":"ping"}"#.to_string(),
        ),
        (
            "id beyond i64",
            r#"{"jsonrpc":"2.0","id":99999999999999999999,"method":"ping"}"#.to_string(),
        ),
        ("missing method", r#"{"jsonrpc":"2.0","id":6}"#.to_string()),
        (
            "empty method",
            r#"{"jsonrpc":"2.0","id":7,"method":""}"#.to_string(),
        ),
        (
            "method and result together",
            r#"{"jsonrpc":"2.0","id":8,"method":"ping","result":{}}"#.to_string(),
        ),
        (
            "scalar params",
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/list","params":"nope"}"#.to_string(),
        ),
        (
            "null params",
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/list","params":null}"#.to_string(),
        ),
    ];

    for (label, raw) in cases {
        server.send_raw(&raw).await;
        let frame = server.next_frame().await;
        assert_eq!(
            frame["error"]["code"], -32600,
            "{label} must be an invalid request: {frame}"
        );
        assert!(
            frame["id"].is_null(),
            "{label} must answer with a null id: {frame}"
        );
    }

    server.send(ping(99)).await;
    assert_eq!(server.next_frame().await["id"], 99);
    server.shutdown().await;
}

/// Deeply nested params must be rejected without unwinding the process, and
/// the loop must keep serving afterwards.
#[tokio::test]
async fn deeply_nested_json_is_rejected_and_the_loop_survives() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    let depth = 2_000;
    let nested = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"echo","arguments":{}{}}}}}"#,
        "[".repeat(depth),
        "]".repeat(depth)
    );
    server.send_raw(&nested).await;
    let frame = server.next_frame().await;
    assert!(
        frame.get("error").is_some(),
        "deep nesting must be refused: {frame}"
    );

    server.send(ping(2)).await;
    let pong = server.next_frame().await;
    assert_eq!(pong["id"], 2, "the loop must keep reading: {pong}");
    server.shutdown().await;
}

/// A multi-megabyte frame is answered whole rather than truncating or
/// desynchronizing the stream.
#[tokio::test]
async fn an_oversized_frame_is_handled_without_desynchronizing_the_stream() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    let payload = "A".repeat(2 * 1024 * 1024);
    server
        .send(call(
            serde_json::json!(1),
            "echo",
            serde_json::json!({"blob": payload}),
        ))
        .await;
    let frame = server.next_frame().await;
    assert_eq!(frame["id"], 1);
    let text = frame["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.len() > 2 * 1024 * 1024,
        "the whole payload must round trip, got {} bytes",
        text.len()
    );

    server.send(ping(2)).await;
    assert_eq!(server.next_frame().await["id"], 2);
    server.shutdown().await;
}

/// Duplicate JSON keys must not let a frame smuggle one method past
/// validation and execute another.
#[tokio::test]
async fn duplicate_json_keys_cannot_smuggle_a_second_method() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server
        .send_raw(r#"{"jsonrpc":"2.0","id":1,"method":"ping","method":"tools/list"}"#)
        .await;
    let frame = server.next_frame().await;
    // Refusing is fine and so is last-key-wins; validating one method while
    // executing the other is not.
    if frame.get("result").is_some() {
        assert!(
            frame["result"].get("tools").is_some() || frame["result"] == serde_json::json!({}),
            "the executed method must be one of the two named: {frame}"
        );
    }
    server.send(ping(2)).await;
    assert_eq!(server.next_frame().await["id"], 2);
    server.shutdown().await;
}

/// Unknown top-level fields are tolerated: peers add extensions, and a strict
/// reject here would break them.
#[tokio::test]
async fn unexpected_top_level_fields_are_tolerated() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server
        .send_raw(r#"{"jsonrpc":"2.0","id":1,"method":"ping","extra":{"nope":true}}"#)
        .await;
    let frame = server.next_frame().await;
    assert_eq!(frame["id"], 1);
    assert!(
        frame.get("result").is_some(),
        "unexpected fields must not fail the request: {frame}"
    );
    server.shutdown().await;
}

/// A BOM-prefixed frame is still served (the #795 regression), including
/// after arbitrary other traffic on the same stream.
#[tokio::test]
async fn a_bom_prefixed_frame_is_accepted() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server.send_raw(&format!("\u{feff}{}", ping(1))).await;
    let frame = server.next_frame().await;
    assert_eq!(frame["id"], 1);
    assert!(
        frame.get("result").is_some(),
        "BOM must be stripped: {frame}"
    );
    server.shutdown().await;
}

/// A batch mixing a valid request with a structurally invalid member must
/// produce an answer, and must never leave the client without one.
#[tokio::test]
async fn a_batch_with_a_broken_member_still_answers() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server
        .send_raw(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","id":2}]"#)
        .await;
    let frame = server.next_frame().await;
    assert!(
        frame.get("error").is_some() || frame.is_array(),
        "a mixed batch must be answered: {frame}"
    );

    server.send(ping(3)).await;
    assert_eq!(server.next_frame().await["id"], 3);
    server.shutdown().await;
}

/// A single byte that is not valid UTF-8 is malformed input from the peer,
/// exactly like malformed JSON. It must not terminate the transport: #797
/// established that a parse error keeps the loop alive, and the byte layer
/// is the same contract one level down (#1271).
#[tokio::test]
async fn invalid_utf8_on_stdin_does_not_kill_the_transport() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server.writer.write_all(&[0xff, 0xfe, b'\n']).await.unwrap();
    server.writer.flush().await.unwrap();
    server.send(ping(1)).await;

    let frame = server
        .next_frame_within(Duration::from_secs(2))
        .await
        .expect("the transport must survive an invalid byte and keep serving");
    // The parse-error frame is optional; being answered at all is the point.
    let frame = if frame["id"].is_null() {
        server.next_frame().await
    } else {
        frame
    };
    assert_eq!(
        frame["id"], 1,
        "the request after the bad byte must be served: {frame}"
    );
    server.shutdown().await;
}

// ============================================================================
// 3. Notifications must never be answered
// ============================================================================

/// Control: a well-formed notification produces no response frame.
#[tokio::test]
async fn a_well_formed_notification_produces_no_response_frame() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server.send(cancel(serde_json::json!(999))).await;
    server.expect_silence(Duration::from_millis(200)).await;
    server.shutdown().await;
}

/// Control: an unrecognised notification method is ignored, not answered.
#[tokio::test]
async fn an_unknown_notification_method_is_ignored() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server
        .send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/bogus"}))
        .await;
    server.expect_silence(Duration::from_millis(200)).await;
    server.shutdown().await;
}

/// A known notification with invalid params is still a notification. JSON-RPC
/// 2.0 is explicit that a server must not reply to one, and the two controls
/// above show the transport gets that right everywhere else.
///
/// Today `process_line` runs `inspect_incoming_value` before it decides
/// whether the frame is a notification, so a validation failure is answered
/// with an unsolicited `-32602` carrying a null id.
#[tokio::test]
async fn a_notification_with_invalid_params_is_not_answered() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server
        .send(serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/cancelled", "params": {}
        }))
        .await;
    server.expect_silence(Duration::from_millis(300)).await;
    server.shutdown().await;
}

/// A response frame is not a request either. A unidirectional stdio server
/// never issues requests, so a stray response is junk it should drop.
///
/// Today it falls through to the request path, fails to match
/// `JsonRpcMessage`, and is answered with `-32700` whose message leaks the
/// internal Rust type name.
#[tokio::test]
async fn a_stray_response_frame_is_not_answered() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server
        .send_raw(r#"{"jsonrpc":"2.0","id":4242,"result":{}}"#)
        .await;
    server.expect_silence(Duration::from_millis(300)).await;
    server.shutdown().await;
}

// ============================================================================
// 4. Ordering
// ============================================================================

/// A second `notifications/initialized` must be ignored, not reset the
/// session or break the connection.
#[tokio::test]
async fn a_repeated_initialized_notification_is_harmless() {
    let mut server = Server::with_router(base_router());
    server.initialize().await;

    server
        .send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    server
        .send(serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .await;
    let frame = server.next_frame().await;
    assert!(
        frame["result"]["tools"].is_array(),
        "the session must survive a repeated initialized: {frame}"
    );
    server.shutdown().await;
}

/// An unsolicited `notifications/initialized` must not open the session. The
/// server has negotiated no protocol version and learned no client
/// capabilities, so the pre-initialize guard still has to hold.
///
/// Today `SessionState::mark_initialized` accepts `Uninitialized ->
/// Initialized` to absorb an HTTP race (#458), and nothing distinguishes that
/// race from a client that simply never sent `initialize`. One notification
/// unlocks the whole surface.
#[tokio::test]
#[ignore = "BUG: SessionState::mark_initialized promotes Uninitialized straight to Initialized, so notifications/initialized alone skips the handshake"]
async fn initialized_before_initialize_does_not_open_the_session() {
    let mut server = Server::with_router(base_router());

    server
        .send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    server
        .send(serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .await;
    let frame = server.next_frame().await;
    assert_eq!(
        frame["error"]["code"], -32600,
        "an unsolicited initialized must not skip the handshake: {frame}"
    );
    server.shutdown().await;
}

/// Control for the test above: without any lifecycle traffic at all, the
/// guard does hold.
#[tokio::test]
async fn a_request_before_initialize_is_refused() {
    let mut server = Server::with_router(base_router());

    server
        .send(serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .await;
    let frame = server.next_frame().await;
    assert_eq!(
        frame["error"]["code"], -32600,
        "tools/list before initialize must be refused: {frame}"
    );

    // ping is the documented exception and stays available.
    server.send(ping(2)).await;
    let pong = server.next_frame().await;
    assert!(pong.get("result").is_some(), "ping must work: {pong}");
    server.shutdown().await;
}

// ============================================================================
// 5. Panicking and misbehaving handlers
// ============================================================================

fn panicking_router(catch: bool) -> McpRouter {
    let boom = ToolBuilder::new("boom")
        .description("Panics")
        .extractor_handler((), |RawArgs(_): RawArgs| async move {
            panic!("handler exploded");
            #[allow(unreachable_code)]
            Ok(CallToolResult::text("unreachable"))
        })
        .build();
    let router = McpRouter::new()
        .server_info("adversarial", "1.0.0")
        .tool(boom)
        .tool(echo_tool());
    if catch { router.catch_panics() } else { router }
}

/// With `catch_panics`, a panicking handler answers with an error result and
/// the connection keeps serving.
#[tokio::test]
async fn a_caught_panic_answers_and_leaves_the_connection_usable() {
    let mut server = Server::with_router(panicking_router(true));
    server.initialize().await;

    server
        .send(call(serde_json::json!(1), "boom", serde_json::json!({})))
        .await;
    let frame = server.next_frame().await;
    assert_eq!(frame["id"], 1);
    assert_eq!(
        frame["result"]["isError"], true,
        "a caught panic is an error result: {frame}"
    );

    server
        .send(call(
            serde_json::json!(2),
            "echo",
            serde_json::json!({"a": 1}),
        ))
        .await;
    assert_eq!(server.next_frame().await["id"], 2);
    server.shutdown().await;
}

/// Without `catch_panics` the panic is deliberately not converted (#1230), so
/// the caller of the panicking request never hears back. The connection must
/// still serve every other request, which is the part that matters.
#[tokio::test]
async fn an_uncaught_panic_strands_its_own_request_but_not_the_transport() {
    let mut server = Server::with_router(panicking_router(false));
    server.initialize().await;

    server
        .send(call(serde_json::json!(1), "boom", serde_json::json!({})))
        .await;
    server
        .send(call(
            serde_json::json!(2),
            "echo",
            serde_json::json!({"a": 1}),
        ))
        .await;

    let frame = server.next_frame().await;
    assert_eq!(
        frame["id"], 2,
        "the surviving request must be answered: {frame}"
    );
    server.expect_silence(Duration::from_millis(300)).await;
    server.shutdown().await;
}

/// A handler that never returns must not stall unrelated requests.
#[tokio::test]
async fn a_hung_handler_does_not_block_other_requests() {
    let hang = ToolBuilder::new("hang")
        .description("Never returns")
        .extractor_handler((), |RawArgs(_): RawArgs| async move {
            std::future::pending::<()>().await;
            Ok(CallToolResult::text("unreachable"))
        })
        .build();
    // A bounded drain: the hung request never finishes, and the default
    // `None` would make the shutdown wait for it forever (#1252).
    let mut server = Server::start(
        StdioTransport::new(
            McpRouter::new()
                .server_info("adversarial", "1.0.0")
                .tool(hang)
                .tool(echo_tool()),
        )
        .drain_timeout(Duration::from_millis(50)),
    );
    server.initialize().await;

    server
        .send(call(serde_json::json!(1), "hang", serde_json::json!({})))
        .await;
    server
        .send(call(
            serde_json::json!(2),
            "echo",
            serde_json::json!({"a": 1}),
        ))
        .await;
    let frame = server.next_frame().await;
    assert_eq!(
        frame["id"], 2,
        "the hung call must not serialize the loop: {frame}"
    );
    server.shutdown().await;
}

/// Under a concurrency bound of one, cancelling the running request must
/// release its slot so the queue behind it drains (#1251, one configuration
/// over from the default).
#[tokio::test]
async fn a_cancelled_request_releases_its_concurrency_slot() {
    let mut server = Server::start(StdioTransport::new(base_router()).max_concurrent_requests(1));
    server.initialize().await;

    server
        .send(call(serde_json::json!(1), "hold", serde_json::json!({})))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    server
        .send(call(
            serde_json::json!(2),
            "echo",
            serde_json::json!({"a": 1}),
        ))
        .await;
    server.send(cancel(serde_json::json!(1))).await;

    let frames = server.next_frames(2).await;
    assert!(
        frames.iter().any(|f| f["id"] == serde_json::json!(2)),
        "the queued request must run once the slot frees: {frames:#?}"
    );
    server.shutdown().await;
}

/// A handler returning a very large payload is delivered intact.
#[tokio::test]
async fn a_huge_handler_payload_is_delivered_intact() {
    let big = ToolBuilder::new("big")
        .description("Returns a large payload")
        .extractor_handler((), |RawArgs(_): RawArgs| async move {
            Ok(CallToolResult::text("B".repeat(4 * 1024 * 1024)))
        })
        .build();
    let mut server = Server::with_router(
        McpRouter::new()
            .server_info("adversarial", "1.0.0")
            .tool(big)
            .tool(echo_tool()),
    );
    server.initialize().await;

    server
        .send(call(serde_json::json!(1), "big", serde_json::json!({})))
        .await;
    let frame = server.next_frame().await;
    assert_eq!(
        frame["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .len(),
        4 * 1024 * 1024
    );
    server.shutdown().await;
}
