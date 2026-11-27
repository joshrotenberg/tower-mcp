//! A server-to-client request the server gives up on is cancelled on the wire
//! (#1705).
//!
//! When a handler waiting on `ctx.elicit_form` or `ctx.sample` is cancelled
//! or times out, the client must receive `notifications/cancelled` naming the
//! server's request, so a host UI can close the question. A request the
//! client already answered, or whose waiter was told it failed, must never
//! be cancelled.
//!
//! Every test reads raw frames, so it sees exactly what a client would, and
//! is deterministic:
//!
//! - abandonment is triggered by the peer (a client cancellation, a released
//!   handler) or by advancing paused time, always after the server's request
//!   has been observed on the wire;
//! - assertions cover every frame through a fixed completion point: end of
//!   output for stdio, end of the POST's SSE stream for HTTP, and for
//!   `ChannelTransport` (whose stream never ends) a marker request written
//!   by the same task that writes cancellations, after any already queued.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};
use tokio::sync::{Notify, mpsc};
use tower_mcp::client::{ChannelTransport, ClientTransport};
use tower_mcp::extract::{Context, RawArgs};
use tower_mcp::protocol::{
    CreateMessageParams, ElicitFormParams, ElicitFormSchema, SamplingMessage,
};
use tower_mcp::{BidirectionalStdioTransport, CallToolResult, McpRouter, ToolBuilder};

const ABANDONED_REASON: &str = "server abandoned the request";
/// Upper bound for any single wait; never reached by a passing test.
const STEP: Duration = Duration::from_secs(5);
/// How many client requests `flood_client_requests` sends: more than the 32
/// events an associated POST's SSE stream buffers.
const FLOOD: usize = 80;
/// The handler's sampling deadline. Real time never reaches it; the timeout
/// tests pause the clock and advance past it.
const SAMPLE_DEADLINE: Duration = Duration::from_secs(60);

/// Test-controlled hooks shared with the tools.
#[derive(Clone)]
struct Hooks {
    /// Releases a handler (or its spawned task) that is holding a request.
    release: Arc<Notify>,
    /// What a spawned task's client request finally resolved to.
    spawned_results: mpsc::UnboundedSender<Result<(), String>>,
}

fn elicit_params() -> ElicitFormParams {
    ElicitFormParams {
        mode: None,
        message: "Continue?".into(),
        requested_schema: ElicitFormSchema::new().boolean_field("confirmed", Some("Confirm"), true),
        meta: None,
    }
}

fn sample_params() -> CreateMessageParams {
    CreateMessageParams::new(vec![SamplingMessage::user("hello")], 10)
}

/// Tools:
/// - `confirm` awaits an elicitation;
/// - `sample_with_timeout` gives up on sampling after [`SAMPLE_DEADLINE`];
/// - `marker` sends the client a `ping` request and waits for the answer;
/// - `spawn_sample` spawns a task that samples until released, reports how
///   that ended, and returns;
/// - `elicit_in_spawned_task_then_return` spawns a task that awaits an
///   elicitation with a cloned context, and returns once released, leaving
///   that task still waiting;
/// - `flood_client_requests` sends [`FLOOD`] pings from spawned tasks, more
///   than an HTTP POST's SSE buffer holds, and waits to be released.
fn router(hooks: Hooks) -> McpRouter {
    let confirm = ToolBuilder::new("confirm")
        .description("Ask the user to confirm")
        .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
            Ok(match ctx.elicit_form(elicit_params()).await {
                Ok(result) => CallToolResult::text(serde_json::to_string(&result.action).unwrap()),
                Err(error) => CallToolResult::error(error.to_string()),
            })
        })
        .build();
    let sample = ToolBuilder::new("sample_with_timeout")
        .description("Sample, giving up after a deadline")
        .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
            Ok(
                match tokio::time::timeout(SAMPLE_DEADLINE, ctx.sample(sample_params())).await {
                    Err(_) => CallToolResult::text("sampling timed out"),
                    Ok(Ok(_)) => CallToolResult::text("sampled"),
                    Ok(Err(error)) => CallToolResult::error(error.to_string()),
                },
            )
        })
        .build();
    let marker = ToolBuilder::new("marker")
        .description("Ping the client")
        .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
            ctx.request_raw("ping", json!({})).await?;
            Ok(CallToolResult::text("marker done"))
        })
        .build();
    let spawn_hooks = hooks.clone();
    let spawn_sample = ToolBuilder::new("spawn_sample")
        .description("Sample from a spawned task until released")
        .extractor_handler((), move |ctx: Context, RawArgs(_): RawArgs| {
            let hooks = spawn_hooks.clone();
            async move {
                tokio::spawn(async move {
                    let outcome = tokio::select! {
                        result = ctx.sample(sample_params()) => {
                            result.map(|_| ()).map_err(|e| e.to_string())
                        }
                        () = hooks.release.notified() => Err("released".to_string()),
                    };
                    let _ = hooks.spawned_results.send(outcome);
                });
                Ok(CallToolResult::text("spawned"))
            }
        })
        .build();
    let elicit_hooks = hooks.clone();
    let hooks_for_flood = hooks;
    let elicit_in_task = ToolBuilder::new("elicit_in_spawned_task_then_return")
        .description("Elicit from a spawned task, return when released")
        .extractor_handler((), move |ctx: Context, RawArgs(_): RawArgs| {
            let hooks = elicit_hooks.clone();
            async move {
                let results = hooks.spawned_results.clone();
                tokio::spawn(async move {
                    let result = ctx.elicit_form(elicit_params()).await;
                    let _ = results.send(result.map(|_| ()).map_err(|e| e.to_string()));
                });
                hooks.release.notified().await;
                Ok(CallToolResult::text("returned"))
            }
        })
        .build();
    let flood_hooks = hooks_for_flood.clone();
    let flood = ToolBuilder::new("flood_client_requests")
        .description("Send the client many pings from spawned tasks, then wait")
        .extractor_handler((), move |ctx: Context, RawArgs(_): RawArgs| {
            let hooks = flood_hooks.clone();
            async move {
                for _ in 0..FLOOD {
                    let ctx = ctx.clone();
                    let results = hooks.spawned_results.clone();
                    tokio::spawn(async move {
                        let result = ctx.request_raw("ping", json!({})).await;
                        let _ = results.send(result.map(|_| ()).map_err(|e| e.to_string()));
                    });
                }
                hooks.release.notified().await;
                Ok(CallToolResult::text("flooded"))
            }
        })
        .build();
    McpRouter::new()
        .server_info("abandon-test", "1.0.0")
        .tool(flood)
        .tool(confirm)
        .tool(sample)
        .tool(marker)
        .tool(spawn_sample)
        .tool(elicit_in_task)
}

fn hooks() -> (Hooks, mpsc::UnboundedReceiver<Result<(), String>>) {
    let (spawned_results, results_rx) = mpsc::unbounded_channel();
    (
        Hooks {
            release: Arc::new(Notify::new()),
            spawned_results,
        },
        results_rx,
    )
}

fn call(id: i64, tool: &str) -> Value {
    json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": {"name": tool, "arguments": {}}
    })
}

fn initialize_request() -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {"sampling": {}, "elicitation": {}},
            "clientInfo": {"name": "abandon-client", "version": "0"}
        }
    })
}

fn is_cancel(frame: &Value) -> bool {
    frame["method"] == "notifications/cancelled"
}

fn is_response_to(frame: &Value, id: i64) -> bool {
    frame["id"] == id && frame.get("method").is_none()
}

fn is_request(frame: &Value, method: &str) -> bool {
    frame["method"] == method && frame.get("id").is_some()
}

fn position(frames: &[Value], want: impl Fn(&Value) -> bool) -> usize {
    frames
        .iter()
        .position(want)
        .unwrap_or_else(|| panic!("frame not found in {frames:#?}"))
}

/// Exactly one cancellation in `frames`, naming `request`, after it.
fn assert_cancelled_once(frames: &[Value], request: &Value) {
    let cancels: Vec<usize> = frames
        .iter()
        .enumerate()
        .filter(|(_, f)| is_cancel(f))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(cancels.len(), 1, "exactly one cancellation: {frames:#?}");
    let cancel = &frames[cancels[0]];
    assert_eq!(cancel["params"]["requestId"], request["id"], "{cancel}");
    assert_eq!(cancel["params"]["reason"], ABANDONED_REASON, "{cancel}");
    let request_at = position(frames, |f| f == request);
    assert!(
        request_at < cancels[0],
        "cancelled after the request: {frames:#?}"
    );
}

fn assert_never_cancelled(frames: &[Value]) {
    assert!(
        !frames.iter().any(is_cancel),
        "nothing may be cancelled: {frames:#?}"
    );
}

/// Stop the clock and move it past the handler's sampling deadline.
async fn expire_sample_deadline() {
    tokio::time::pause();
    tokio::time::advance(SAMPLE_DEADLINE + Duration::from_secs(1)).await;
}

// ============================================================================
// stdio and ChannelTransport
// ============================================================================

enum Wire {
    Stdio {
        input: Option<DuplexStream>,
        output: Lines<BufReader<DuplexStream>>,
        server: tokio::task::JoinHandle<tower_mcp::error::Result<()>>,
    },
    Channel(ChannelTransport),
}

/// A raw client that records every frame it receives.
struct Peer {
    wire: Wire,
    frames: Vec<Value>,
}

impl Peer {
    fn stdio(router: McpRouter) -> Self {
        Self::stdio_transport(BidirectionalStdioTransport::new(router))
    }

    fn stdio_transport(mut transport: BidirectionalStdioTransport) -> Self {
        let (input, reader) = tokio::io::duplex(1 << 16);
        let (writer, output) = tokio::io::duplex(1 << 16);
        let server = tokio::spawn(async move { transport.run_with_streams(reader, writer).await });
        Peer {
            wire: Wire::Stdio {
                input: Some(input),
                output: BufReader::new(output).lines(),
                server,
            },
            frames: Vec::new(),
        }
    }

    fn channel(router: McpRouter) -> Self {
        Peer {
            wire: Wire::Channel(ChannelTransport::new(router)),
            frames: Vec::new(),
        }
    }

    async fn send(&mut self, frame: Value) {
        match &mut self.wire {
            Wire::Stdio { input, .. } => input
                .as_mut()
                .expect("input still open")
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .unwrap(),
            Wire::Channel(transport) => transport.send(&frame.to_string()).await.unwrap(),
        }
    }

    /// The next frame, or `None` at end of output.
    async fn recv(&mut self) -> Option<Value> {
        let line = match &mut self.wire {
            // `next_line` is cancel safe, so the deadline loses no input.
            Wire::Stdio { output, .. } => tokio::time::timeout(STEP, output.next_line())
                .await
                .expect("server went silent")
                .unwrap(),
            Wire::Channel(transport) => tokio::time::timeout(STEP, transport.recv())
                .await
                .expect("server went silent")
                .unwrap(),
        }?;
        let frame: Value = serde_json::from_str(&line).unwrap();
        self.frames.push(frame.clone());
        Some(frame)
    }

    async fn expect(&mut self, what: &str, want: impl Fn(&Value) -> bool) -> Value {
        loop {
            let frame = self
                .recv()
                .await
                .unwrap_or_else(|| panic!("output ended before {what}"));
            if want(&frame) {
                return frame;
            }
        }
    }

    async fn handshake(&mut self) {
        self.send(initialize_request()).await;
        self.expect("initialize response", |f| is_response_to(f, 1))
            .await;
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
    }

    /// Read through the completion point and return every frame received.
    async fn finish(mut self) -> Vec<Value> {
        match &mut self.wire {
            Wire::Stdio { input, server, .. } => {
                // End of input stops the transport, which writes everything
                // it has and then closes its output.
                drop(input.take());
                tokio::time::timeout(STEP, server)
                    .await
                    .expect("transport stops at end of input")
                    .unwrap()
                    .unwrap();
                while self.recv().await.is_some() {}
            }
            Wire::Channel(_) => {
                // The marker ping is written by the task that writes
                // cancellations, after any it has already been handed.
                self.send(call(99, "marker")).await;
                let ping = self.expect("marker ping", |f| is_request(f, "ping")).await;
                self.send(json!({"jsonrpc": "2.0", "id": ping["id"], "result": {}}))
                    .await;
                self.expect("marker response", |f| is_response_to(f, 99))
                    .await;
            }
        }
        self.frames
    }
}

/// The client cancels the tool call; the handler's elicitation is cancelled
/// in turn, with the elicitation's own id.
async fn cancelled_handler_cancels_its_elicitation(mut peer: Peer) {
    peer.handshake().await;
    peer.send(call(2, "confirm")).await;
    let elicit = peer
        .expect("elicitation/create", |f| {
            is_request(f, "elicitation/create")
        })
        .await;

    peer.send(json!({
        "jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": {"requestId": 2, "reason": "user closed the tool call"}
    }))
    .await;
    peer.expect("notifications/cancelled", is_cancel).await;

    assert_cancelled_once(&peer.finish().await, &elicit);
}

/// The handler's deadline passes; the sampling request is cancelled, and a
/// late answer to it does not disturb the connection.
async fn sample_timeout_cancels_the_request(mut peer: Peer) {
    peer.handshake().await;
    peer.send(call(2, "sample_with_timeout")).await;
    let sample = peer
        .expect("sampling/createMessage", |f| {
            is_request(f, "sampling/createMessage")
        })
        .await;

    expire_sample_deadline().await;
    peer.expect("notifications/cancelled", is_cancel).await;
    let response = match peer.frames.iter().find(|f| is_response_to(f, 2)) {
        Some(response) => response.clone(),
        None => peer.expect("tool response", |f| is_response_to(f, 2)).await,
    };
    assert_eq!(
        response["result"]["content"][0]["text"],
        "sampling timed out"
    );

    peer.send(json!({
        "jsonrpc": "2.0", "id": sample["id"],
        "result": {"role": "assistant", "content": {"type": "text", "text": "late"}, "model": "m"}
    }))
    .await;

    assert_cancelled_once(&peer.finish().await, &sample);
}

/// An answered elicitation is never cancelled.
async fn answered_elicitation_is_not_cancelled(mut peer: Peer) {
    peer.handshake().await;
    peer.send(call(2, "confirm")).await;
    let elicit = peer
        .expect("elicitation/create", |f| {
            is_request(f, "elicitation/create")
        })
        .await;
    peer.send(json!({
        "jsonrpc": "2.0", "id": elicit["id"],
        "result": {"action": "accept", "content": {"confirmed": true}}
    }))
    .await;
    let response = peer.expect("tool response", |f| is_response_to(f, 2)).await;
    assert_eq!(response["result"]["content"][0]["text"], "\"accept\"");

    assert_never_cancelled(&peer.finish().await);
}

#[tokio::test]
async fn stdio_cancelled_handler_cancels_its_elicitation() {
    cancelled_handler_cancels_its_elicitation(Peer::stdio(router(hooks().0))).await;
}

#[tokio::test]
async fn stdio_sample_timeout_cancels_the_request() {
    sample_timeout_cancels_the_request(Peer::stdio(router(hooks().0))).await;
}

#[tokio::test]
async fn stdio_answered_elicitation_is_not_cancelled() {
    answered_elicitation_is_not_cancelled(Peer::stdio(router(hooks().0))).await;
}

// ChannelTransport does not forward a client's `notifications/cancelled` to
// the router, so the client-cancel case has no channel variant; the
// handler-side timeout covers abandonment there.

#[tokio::test]
async fn channel_sample_timeout_cancels_the_request() {
    sample_timeout_cancels_the_request(Peer::channel(router(hooks().0))).await;
}

#[tokio::test]
async fn channel_answered_elicitation_is_not_cancelled() {
    answered_elicitation_is_not_cancelled(Peer::channel(router(hooks().0))).await;
}

/// Start `elicit_in_spawned_task_then_return` and wait until its request is
/// on the wire. The spawned task keeps waiting on the client.
async fn leave_a_request_waiting(peer: &mut Peer) {
    peer.handshake().await;
    peer.send(call(2, "elicit_in_spawned_task_then_return"))
        .await;
    peer.expect("elicitation/create", |f| {
        is_request(f, "elicitation/create")
    })
    .await;
}

async fn assert_released(results: &mut mpsc::UnboundedReceiver<Result<(), String>>) {
    let result = tokio::time::timeout(STEP, results.recv())
        .await
        .expect("the waiting request is released")
        .unwrap();
    assert!(result.is_err(), "released with an error, got {result:?}");
}

/// When the in-process client goes away, a handler waiting on it is
/// released instead of waiting forever.
#[tokio::test]
async fn channel_client_drop_releases_a_waiting_request() {
    let (hooks, mut results) = hooks();
    let mut peer = Peer::channel(router(hooks));
    leave_a_request_waiting(&mut peer).await;
    drop(peer);
    assert_released(&mut results).await;
}

/// Dropping a running stdio transport future releases a handler waiting on
/// the client, even though the transport itself (and its registry) lives on.
#[tokio::test]
async fn stdio_dropped_run_future_releases_a_waiting_request() {
    let (hooks, mut results) = hooks();
    let mut transport = BidirectionalStdioTransport::new(router(hooks));
    let (input, reader) = tokio::io::duplex(1 << 16);
    let (writer, output) = tokio::io::duplex(1 << 16);
    let stop = Arc::new(Notify::new());
    let (kept_tx, _kept_rx) = tokio::sync::oneshot::channel();
    let stopping = stop.clone();
    let server = tokio::spawn(async move {
        tokio::select! {
            _ = transport.run_with_streams(reader, writer) => {}
            () = stopping.notified() => {}
        }
        // Keep the transport alive past its dropped run future.
        let _ = kept_tx.send(transport);
        Ok(())
    });
    let mut peer = Peer {
        wire: Wire::Stdio {
            input: Some(input),
            output: BufReader::new(output).lines(),
            server,
        },
        frames: Vec::new(),
    };
    leave_a_request_waiting(&mut peer).await;

    stop.notify_one();
    assert_released(&mut results).await;
}

/// An input and output that fail at once, like pipes that are already
/// broken.
struct Broken;

impl tokio::io::AsyncRead for Broken {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
    }
}

impl tokio::io::AsyncWrite for Broken {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// A run that fails with an I/O error releases a request queued for it,
/// while the transport itself lives on.
#[tokio::test]
async fn stdio_failed_run_releases_a_queued_request() {
    let mut transport = BidirectionalStdioTransport::new(router(hooks().0));
    let requester = transport.client_requester();
    let mut request = Box::pin(requester.request("ping".to_string(), json!({})));
    assert!(futures::poll!(request.as_mut()).is_pending(), "queued");

    assert!(transport.run_with_streams(Broken, Broken).await.is_err());

    let result = tokio::time::timeout(STEP, request)
        .await
        .expect("the queued request is released");
    assert!(result.is_err(), "{result:?}");
    drop(transport);
}

/// A cancellation produced during one run belongs to that run's peer. If
/// the run ends before writing it, a later run, with a different peer, must
/// not write it.
#[tokio::test]
async fn stdio_cancellation_does_not_leak_into_the_next_run() {
    let mut transport = BidirectionalStdioTransport::new(router(hooks().0));
    let requester = transport.client_requester();

    {
        let (mut input, reader) = tokio::io::duplex(1 << 16);
        let (writer, output) = tokio::io::duplex(1 << 16);
        let mut output = BufReader::new(output).lines();
        let run = transport.run_with_streams(reader, writer);
        tokio::pin!(run);
        let mut next_frame = async |run: &mut std::pin::Pin<&mut _>| -> Value {
            let line = tokio::select! {
                _ = run => panic!("the run ended early"),
                line = tokio::time::timeout(STEP, output.next_line()) => {
                    line.expect("frame arrives").unwrap().unwrap()
                }
            };
            serde_json::from_str(&line).unwrap()
        };

        // Open a legacy session, as a real client would.
        input
            .write_all(format!("{}\n", initialize_request()).as_bytes())
            .await
            .unwrap();
        let initialized = next_frame(&mut run).await;
        assert!(is_response_to(&initialized, 1), "{initialized}");

        let mut request = Box::pin(requester.request("ping".to_string(), json!({})));
        assert!(futures::poll!(request.as_mut()).is_pending());
        // Drive the run until the request is on this peer's wire.
        let frame = next_frame(&mut run).await;
        assert!(is_request(&frame, "ping"), "{frame}");

        // The waiter gives up and the registry reports it, but the run is
        // never polled again to write the cancellation.
        drop(request);
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    let (input, reader) = tokio::io::duplex(1 << 16);
    let (writer, output) = tokio::io::duplex(1 << 16);
    drop(input);
    transport.run_with_streams(reader, writer).await.unwrap();
    let mut output = BufReader::new(output).lines();
    let mut frames = Vec::new();
    while let Some(line) = output.next_line().await.unwrap() {
        frames.push(serde_json::from_str::<Value>(&line).unwrap());
    }
    assert_never_cancelled(&frames);
}

/// `close()` on a retained ChannelTransport releases a handler waiting on
/// the client, without waiting for the transport to be dropped.
#[tokio::test]
async fn channel_close_releases_a_waiting_request() {
    let (hooks, mut results) = hooks();
    let mut peer = Peer::channel(router(hooks));
    leave_a_request_waiting(&mut peer).await;
    let Wire::Channel(transport) = &mut peer.wire else {
        unreachable!()
    };
    transport.close().await.unwrap();
    assert_released(&mut results).await;
    drop(peer);
}

#[cfg(all(feature = "protocol-2026-07-28", feature = "stateless"))]
fn mixed_transport() -> BidirectionalStdioTransport {
    BidirectionalStdioTransport::new(router(hooks().0))
        .protocol_versions(["2025-11-25", "2026-07-28"])
        .unwrap()
}

/// With both revisions enabled, a connection that has carried 2026-07-28
/// traffic without a legacy `initialize` is on the final lifecycle, so the
/// public requester refuses to send anything.
#[cfg(all(feature = "protocol-2026-07-28", feature = "stateless"))]
#[tokio::test]
async fn stdio_mixed_transport_refuses_direct_requests_on_final_traffic() {
    let transport = mixed_transport();
    let requester = transport.client_requester();
    let mut peer = Peer::stdio_transport(transport);

    peer.send(json!({
        "jsonrpc": "2.0", "id": 5, "method": "tools/list",
        "params": {"_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {"name": "final-client", "version": "0"},
            "io.modelcontextprotocol/clientCapabilities": {}
        }}
    }))
    .await;
    let listed = peer
        .expect("final tools/list response", |f| is_response_to(f, 5))
        .await;
    assert!(listed["result"]["tools"].is_array(), "{listed}");

    let error = tokio::time::timeout(STEP, requester.request("ping".to_string(), json!({})))
        .await
        .expect("refused promptly")
        .expect_err("no legacy lifecycle, no server-initiated request");
    assert!(error.to_string().contains("not on a legacy"), "{error}");

    let frames = peer.finish().await;
    assert_eq!(frames.len(), 1, "only the tools/list response: {frames:#?}");
}

/// With both revisions enabled and no traffic yet, the public requester
/// works as it always has: the request is written and answered.
#[cfg(all(feature = "protocol-2026-07-28", feature = "stateless"))]
#[tokio::test]
async fn stdio_mixed_transport_allows_direct_requests_before_any_traffic() {
    let transport = mixed_transport();
    let requester = transport.client_requester();
    let mut peer = Peer::stdio_transport(transport);

    let request =
        tokio::spawn(async move { requester.request("ping".to_string(), json!({})).await });
    let ping = peer.expect("ping request", |f| is_request(f, "ping")).await;
    peer.send(json!({"jsonrpc": "2.0", "id": ping["id"], "result": {}}))
        .await;
    tokio::time::timeout(STEP, request)
        .await
        .expect("answered promptly")
        .unwrap()
        .expect("no traffic yet, so the request is permitted");

    let frames = peer.finish().await;
    assert_eq!(frames.len(), 1, "only the ping request: {frames:#?}");
    assert_never_cancelled(&frames);
}

/// With both revisions enabled, a legacy `initialize` establishes the
/// lifecycle, and the public requester then works as before.
#[cfg(all(feature = "protocol-2026-07-28", feature = "stateless"))]
#[tokio::test]
async fn stdio_mixed_transport_allows_direct_requests_after_legacy_initialize() {
    let transport = mixed_transport();
    let requester = transport.client_requester();
    let mut peer = Peer::stdio_transport(transport);
    peer.handshake().await;

    let request =
        tokio::spawn(async move { requester.request("ping".to_string(), json!({})).await });
    let ping = peer.expect("ping request", |f| is_request(f, "ping")).await;
    peer.send(json!({"jsonrpc": "2.0", "id": ping["id"], "result": {}}))
        .await;
    tokio::time::timeout(STEP, request)
        .await
        .expect("answered promptly")
        .unwrap()
        .expect("the legacy connection permits the request");

    assert_never_cancelled(&peer.finish().await);
}

/// The 2026-07-28 protocol forbids server-initiated requests. The public
/// stdio requester must refuse to send one when the transport accepts no
/// legacy version, so nothing (neither a request nor a cancellation)
/// reaches the wire.
#[cfg(feature = "protocol-2026-07-28")]
#[tokio::test]
async fn stdio_final_only_transport_refuses_direct_client_requests() {
    let transport = BidirectionalStdioTransport::new(router(hooks().0))
        .protocol_versions(["2026-07-28"])
        .unwrap();
    let requester = transport.client_requester();
    let peer = Peer::stdio_transport(transport);

    let error = tokio::time::timeout(STEP, requester.request("ping".to_string(), json!({})))
        .await
        .expect("refused promptly")
        .expect_err("final-only transport refuses server-initiated requests");
    assert!(
        error.to_string().contains("2026-07-28 forbids"),
        "unexpected error: {error}"
    );
    // A request dropped before it could be sent leaves no trace either.
    drop(
        tokio::time::timeout(
            Duration::from_millis(1),
            requester.request("sampling/createMessage".to_string(), json!({})),
        )
        .await,
    );

    let frames = peer.finish().await;
    assert!(frames.is_empty(), "nothing may be written: {frames:#?}");
}

// ============================================================================
// HTTP (2025-11-25 sessions): requests and cancellations ride the
// originating POST's SSE stream
// ============================================================================

#[cfg(feature = "http")]
mod http {
    use super::*;

    use axum::body::{Body, BodyDataStream};
    use axum::http::Request;
    use futures::StreamExt;
    use tower::ServiceExt;
    use tower_mcp::HttpTransport;

    fn app(hooks: Hooks) -> axum::Router {
        HttpTransport::new(router(hooks))
            .with_sampling()
            .disable_origin_validation()
            .into_router()
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        body: Option<Value>,
        session: Option<&str>,
    ) -> axum::http::Response<Body> {
        let mut request = Request::builder()
            .method(method)
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
        app.clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap()
    }

    async fn post(app: &axum::Router, body: Value, session: &str) -> axum::http::Response<Body> {
        send(app, "POST", Some(body), Some(session)).await
    }

    /// Every JSON-RPC frame on one POST's SSE response stream, as read.
    pub(super) struct SseFrames<S> {
        body: S,
        buffer: String,
        pub(super) frames: Vec<Value>,
    }

    impl SseFrames<BodyDataStream> {
        fn new(response: axum::http::Response<Body>) -> Self {
            assert_eq!(response.status(), 200);
            SseFrames::from_stream(response.into_body().into_data_stream())
        }
    }

    impl<S, B, E> SseFrames<S>
    where
        S: futures::Stream<Item = Result<B, E>> + Unpin,
        B: AsRef<[u8]>,
        E: std::fmt::Debug,
    {
        pub(super) fn from_stream(body: S) -> Self {
            Self {
                body,
                buffer: String::new(),
                frames: Vec::new(),
            }
        }

        /// The next frame, or `None` once the stream ends.
        pub(super) async fn next(&mut self) -> Option<Value> {
            loop {
                if let Some(end) = self.buffer.find("\n\n") {
                    let event: String = self.buffer.drain(..end + 2).collect();
                    let data: Vec<&str> = event
                        .lines()
                        .filter_map(|line| line.strip_prefix("data:"))
                        .map(str::trim_start)
                        .collect();
                    if data.is_empty() {
                        continue; // keep-alive comment
                    }
                    let frame: Value = serde_json::from_str(&data.join("\n")).unwrap();
                    self.frames.push(frame.clone());
                    return Some(frame);
                }
                let chunk = tokio::time::timeout(STEP, self.body.next())
                    .await
                    .expect("SSE stream stalled")?
                    .unwrap();
                self.buffer
                    .push_str(&String::from_utf8_lossy(chunk.as_ref()));
            }
        }

        pub(super) async fn expect(&mut self, what: &str, want: impl Fn(&Value) -> bool) -> Value {
            loop {
                let frame = self
                    .next()
                    .await
                    .unwrap_or_else(|| panic!("stream ended before {what}"));
                if want(&frame) {
                    return frame;
                }
            }
        }

        /// Read to the end of the stream and return every frame it carried.
        pub(super) async fn finish(mut self) -> Vec<Value> {
            while self.next().await.is_some() {}
            self.frames
        }
    }

    async fn initialize(app: &axum::Router) -> String {
        let response = send(app, "POST", Some(initialize_request()), None).await;
        let session = response
            .headers()
            .get("mcp-session-id")
            .expect("session id")
            .to_str()
            .unwrap()
            .to_string();
        post(
            app,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            &session,
        )
        .await;
        session
    }

    #[tokio::test]
    async fn http_cancelled_handler_cancels_its_elicitation() {
        let app = app(hooks().0);
        let session = initialize(&app).await;
        let mut stream = SseFrames::new(post(&app, call(2, "confirm"), &session).await);
        let elicit = stream
            .expect("elicitation/create", |f| {
                is_request(f, "elicitation/create")
            })
            .await;

        let accepted = post(
            &app,
            json!({
                "jsonrpc": "2.0", "method": "notifications/cancelled",
                "params": {"requestId": 2, "reason": "user closed the tool call"}
            }),
            &session,
        )
        .await;
        assert_eq!(accepted.status(), 202);

        assert_cancelled_once(&stream.finish().await, &elicit);
    }

    #[tokio::test]
    async fn http_sample_timeout_cancels_before_the_response() {
        let app = app(hooks().0);
        let session = initialize(&app).await;
        let mut stream = SseFrames::new(post(&app, call(2, "sample_with_timeout"), &session).await);
        let sample = stream
            .expect("sampling/createMessage", |f| {
                is_request(f, "sampling/createMessage")
            })
            .await;

        expire_sample_deadline().await;
        let frames = stream.finish().await;

        assert_cancelled_once(&frames, &sample);
        let cancel_at = position(&frames, is_cancel);
        let response_at = position(&frames, |f| is_response_to(f, 2));
        assert!(
            cancel_at < response_at,
            "cancelled before the response: {frames:#?}"
        );
        assert_eq!(
            frames[response_at]["result"]["content"][0]["text"],
            "sampling timed out"
        );

        // A late response to the cancelled request is accepted and ignored.
        let late = post(
            &app,
            json!({
                "jsonrpc": "2.0", "id": sample["id"],
                "result": {"role": "assistant", "content": {"type": "text", "text": "late"}, "model": "m"}
            }),
            &session,
        )
        .await;
        assert_eq!(late.status(), 202);
    }

    #[tokio::test]
    async fn http_answered_elicitation_is_not_cancelled() {
        let app = app(hooks().0);
        let session = initialize(&app).await;
        let mut stream = SseFrames::new(post(&app, call(2, "confirm"), &session).await);
        let elicit = stream
            .expect("elicitation/create", |f| {
                is_request(f, "elicitation/create")
            })
            .await;

        let accepted = post(
            &app,
            json!({
                "jsonrpc": "2.0", "id": elicit["id"],
                "result": {"action": "accept", "content": {"confirmed": true}}
            }),
            &session,
        )
        .await;
        assert_eq!(accepted.status(), 202);

        let frames = stream.finish().await;
        let response = &frames[position(&frames, |f| is_response_to(f, 2))];
        assert_eq!(response["result"]["content"][0]["text"], "\"accept\"");
        assert_never_cancelled(&frames);
    }

    /// The POST handler returns while a task holding a cloned context still
    /// waits on its elicitation. That request is failed for the task and not
    /// cancelled on the wire: one outcome, never both.
    #[tokio::test]
    async fn http_request_still_awaited_at_post_completion_is_failed_not_cancelled() {
        let (hooks, mut spawned_results) = hooks();
        let app = app(hooks.clone());
        let session = initialize(&app).await;
        let mut stream = SseFrames::new(
            post(
                &app,
                call(2, "elicit_in_spawned_task_then_return"),
                &session,
            )
            .await,
        );
        stream
            .expect("elicitation/create", |f| {
                is_request(f, "elicitation/create")
            })
            .await;

        hooks.release.notify_one();
        let frames = stream.finish().await;

        let response = &frames[position(&frames, |f| is_response_to(f, 2))];
        assert_eq!(response["result"]["content"][0]["text"], "returned");
        assert_never_cancelled(&frames);
        let spawned = tokio::time::timeout(STEP, spawned_results.recv())
            .await
            .expect("the spawned task's request resolves")
            .unwrap();
        let error = spawned.expect_err("the waiting task is told the request failed");
        assert!(
            error.contains("originating POST completed"),
            "unexpected error: {error}"
        );
    }

    /// Deleting the session ends a POST that is still waiting on the client,
    /// and releases its handler, instead of leaving both open.
    #[tokio::test]
    async fn http_session_termination_ends_a_waiting_post() {
        let app = app(hooks().0);
        let session = initialize(&app).await;
        let mut stream = SseFrames::new(post(&app, call(2, "confirm"), &session).await);
        stream
            .expect("elicitation/create", |f| {
                is_request(f, "elicitation/create")
            })
            .await;

        let deleted = send(&app, "DELETE", None, Some(&session)).await;
        assert_eq!(deleted.status(), 200);

        let frames = stream.finish().await;
        assert!(
            !frames.iter().any(|f| is_response_to(f, 2)),
            "the POST ends without an answer: {frames:#?}"
        );
        assert_never_cancelled(&frames);
    }

    /// Session termination wins even while the POST's writer is parked on a
    /// full SSE buffer, and every client request the POST issued, written or
    /// still queued, is released.
    #[tokio::test]
    async fn http_session_termination_releases_a_post_parked_on_a_full_buffer() {
        let (hooks, mut results) = hooks();
        let app = app(hooks);
        let session = initialize(&app).await;
        // Hold the response without reading it, so its buffer fills.
        let response = post(&app, call(2, "flood_client_requests"), &session).await;
        assert_eq!(response.status(), 200);

        // Paused time advances only once every task is idle, so this returns
        // when the writer is parked on the full buffer and the remaining
        // requests are queued behind it.
        tokio::time::pause();
        tokio::time::sleep(Duration::from_secs(1)).await;

        let deleted = send(&app, "DELETE", None, Some(&session)).await;
        assert_eq!(deleted.status(), 200);

        let mut terminated = 0;
        for _ in 0..FLOOD {
            let result = tokio::time::timeout(STEP, results.recv())
                .await
                .expect("every client request is released")
                .unwrap();
            let error = result.expect_err("no client answered");
            if error.contains("session terminated") {
                terminated += 1;
            }
        }
        // Requests already written were released by the POST's cleanup.
        assert!(terminated > 0, "some requests had been written");

        let frames = SseFrames::new(response).finish().await;
        assert!(
            frames.len() < FLOOD,
            "the buffer was full: {}",
            frames.len()
        );
        assert!(frames.iter().all(|f| is_request(f, "ping")), "{frames:#?}");
    }
}

// ============================================================================
// WebSocket (binds a local socket)
// ============================================================================

/// The bidirectional WebSocket loop dispatches each request inline, so a
/// handler that awaits sampling directly cannot have its request written
/// until it returns (a pre-existing limitation). These tests sample from a
/// task the handler spawns, which is the supported pattern there.
#[cfg(feature = "websocket")]
mod websocket {
    use super::*;

    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::protocol::Message;
    use tower_mcp::WebSocketTransport;

    type RawWs = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn spawned_outcome(
        outcomes: &mut mpsc::UnboundedReceiver<Result<(), String>>,
    ) -> Result<(), String> {
        tokio::time::timeout(STEP, outcomes.recv())
            .await
            .expect("the spawned sampling task finishes")
            .unwrap()
    }

    struct Socket {
        ws: RawWs,
        frames: Vec<Value>,
    }

    impl Socket {
        async fn connect(router: McpRouter) -> Self {
            let app = WebSocketTransport::new(router)
                .with_sampling()
                .into_router();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
                .await
                .expect("connect");
            Self {
                ws,
                frames: Vec::new(),
            }
        }

        async fn send(&mut self, frame: Value) {
            self.ws
                .send(Message::Text(frame.to_string().into()))
                .await
                .unwrap();
        }

        async fn expect(&mut self, what: &str, want: impl Fn(&Value) -> bool) -> Value {
            loop {
                let message = tokio::time::timeout(STEP, self.ws.next())
                    .await
                    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
                let text = match message {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(_)) => continue,
                    other => panic!("socket ended before {what}: {other:?}"),
                };
                let frame: Value = serde_json::from_str(&text).unwrap();
                self.frames.push(frame.clone());
                if want(&frame) {
                    return frame;
                }
            }
        }

        async fn handshake(&mut self) {
            self.send(initialize_request()).await;
            self.expect("initialize response", |f| is_response_to(f, 1))
                .await;
            self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .await;
        }

        /// Requests are handled in order on the connection's loop, which
        /// also writes cancellations; a ping answered after a cancellation
        /// was queued shows everything before it.
        async fn finish(mut self) -> Vec<Value> {
            self.send(json!({"jsonrpc": "2.0", "id": 99, "method": "ping"}))
                .await;
            self.expect("ping response", |f| is_response_to(f, 99))
                .await;
            self.frames
        }
    }

    #[tokio::test]
    async fn websocket_abandoned_sampling_is_cancelled() {
        let (hooks, mut outcomes) = hooks();
        let mut socket = Socket::connect(router(hooks.clone())).await;
        socket.handshake().await;

        socket.send(call(2, "spawn_sample")).await;
        let sample = socket
            .expect("sampling/createMessage", |f| {
                is_request(f, "sampling/createMessage")
            })
            .await;
        // Only now, with the request on the wire, does the task give up.
        hooks.release.notify_one();
        assert_eq!(spawned_outcome(&mut outcomes).await, Err("released".into()));
        socket.expect("notifications/cancelled", is_cancel).await;

        let frames = socket.finish().await;
        assert_cancelled_once(&frames, &sample);
        let response = &frames[position(&frames, |f| is_response_to(f, 2))];
        assert_eq!(response["result"]["content"][0]["text"], "spawned");
    }

    #[tokio::test]
    async fn websocket_answered_sampling_is_not_cancelled() {
        let (hooks, mut outcomes) = hooks();
        let mut socket = Socket::connect(router(hooks.clone())).await;
        socket.handshake().await;

        socket.send(call(2, "spawn_sample")).await;
        let sample = socket
            .expect("sampling/createMessage", |f| {
                is_request(f, "sampling/createMessage")
            })
            .await;
        socket
            .send(json!({
                "jsonrpc": "2.0", "id": sample["id"],
                "result": {"role": "assistant", "content": {"type": "text", "text": "hi"}, "model": "m"}
            }))
            .await;
        // Sending the reply does not mean the server has read it. Wait until
        // the task has its answer; only then can nothing abandon the request.
        // The task is never released.
        assert_eq!(spawned_outcome(&mut outcomes).await, Ok(()));

        assert_never_cancelled(&socket.finish().await);
    }
}
