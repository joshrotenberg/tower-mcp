//! End-to-end integration tests for the stdio transport read-eval-write loop.
//!
//! These tests drive [`StdioTransport`] and [`BidirectionalStdioTransport`]
//! with in-memory `tokio::io::duplex()` streams via their `run_with_streams`
//! entrypoints. That capability is what makes loop-level assertions possible
//! at all -- the lib-level tests in `transport::stdio::tests` can only
//! exercise helpers like `parse_error_response()` and `process_line()`, not
//! the actual loop that wires them together.
//!
//! Regression coverage for #797 (bidi closed instead of writing a parse-error
//! response) and follow-up to #812.

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::{Duration, timeout};
use tower_mcp::extract::{Context, Json};
use tower_mcp::protocol::{ElicitAction, ElicitFormParams, ElicitFormSchema};
use tower_mcp::transport::stdio::BidirectionalStdioTransport;
use tower_mcp::{CallToolResult, McpRouter, StdioTransport, ToolBuilder};
use tower_mcp_types::testing::assert_jsonrpc_error_response;

/// Build a minimal router used for the e2e tests. `ping` works without
/// session initialization, so tests can call it on a fresh transport.
fn router() -> McpRouter {
    McpRouter::new().server_info("stdio-loop-test", "0.0.0")
}

/// Read newline-delimited JSON-RPC frames from `reader` until either the
/// expected count is reached or EOF is observed. Returns the parsed
/// `serde_json::Value` for each frame in order.
async fn read_n_frames<R>(mut reader: BufReader<R>, expected: usize) -> Vec<serde_json::Value>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut out = Vec::with_capacity(expected);
    while out.len() < expected {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .expect("read from server output");
        if n == 0 {
            break; // EOF before we got everything
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(trimmed)
            .unwrap_or_else(|e| panic!("invalid JSON on output: {e}: {trimmed}"));
        out.push(v);
    }
    out
}

// ============================================================================
// StdioTransport
// ============================================================================

/// Test 1: malformed JSON on stdin produces a JSON-RPC parse-error frame
/// (`-32700`, `id: null`) on stdout, and the response is a valid wire frame.
#[tokio::test]
async fn stdio_transport_parse_error_wire_shape() {
    let mut transport = StdioTransport::new(router());

    let (server_stdin_writer, server_stdin) = tokio::io::duplex(4096);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(4096);

    // Drive the loop in the background.
    let handle = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
    });

    // Write malformed input, then close the writer so the server hits EOF.
    let mut stdin_writer = server_stdin_writer;
    stdin_writer
        .write_all(b"not valid json{{{\n")
        .await
        .unwrap();
    stdin_writer.flush().await.unwrap();
    drop(stdin_writer); // EOF -> loop exits cleanly

    let reader = BufReader::new(server_stdout_reader);
    let frames = read_n_frames(reader, 1).await;
    handle
        .await
        .expect("transport task join")
        .expect("run_with_streams ok");

    assert_eq!(
        frames.len(),
        1,
        "expected one parse-error frame, got: {frames:?}"
    );
    let frame = &frames[0];
    assert_jsonrpc_error_response(frame);
    assert!(
        frame["id"].is_null(),
        "parse error id must be null, got: {frame}"
    );
    assert_eq!(frame["error"]["code"].as_i64().unwrap(), -32700);
}

/// Test 2 (#797 regression): a parse error must NOT close the loop. The
/// server must keep reading and respond to subsequent valid input on the
/// same stream.
#[tokio::test]
async fn stdio_transport_loop_continues_after_parse_error() {
    let mut transport = StdioTransport::new(router());

    let (server_stdin_writer, server_stdin) = tokio::io::duplex(4096);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(4096);

    let handle = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
    });

    let mut stdin_writer = server_stdin_writer;
    // First: malformed JSON -- should produce a -32700 parse error
    stdin_writer.write_all(b"this is not json\n").await.unwrap();
    // Then: a valid ping request on the same stream -- should be processed
    stdin_writer
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"ping\"}\n")
        .await
        .unwrap();
    stdin_writer.flush().await.unwrap();
    drop(stdin_writer);

    let reader = BufReader::new(server_stdout_reader);
    let frames = read_n_frames(reader, 2).await;
    handle
        .await
        .expect("transport task join")
        .expect("run_with_streams ok");

    assert_eq!(
        frames.len(),
        2,
        "expected parse-error frame + ping response, got: {frames:?}"
    );

    // Frame 1: the parse error
    assert_jsonrpc_error_response(&frames[0]);
    assert!(frames[0]["id"].is_null());
    assert_eq!(frames[0]["error"]["code"].as_i64().unwrap(), -32700);

    // Frame 2: the ping response that proves the loop kept running
    assert_eq!(frames[1]["jsonrpc"], "2.0");
    assert_eq!(frames[1]["id"], 42);
    assert!(
        frames[1].get("result").is_some(),
        "ping must return a successful result frame, got: {}",
        frames[1]
    );
}

/// Test 3: closing the writer side of the input stream (EOF) cleanly
/// terminates the loop -- `run_with_streams` returns `Ok(())`.
#[tokio::test]
async fn stdio_transport_eof_returns_ok() {
    let mut transport = StdioTransport::new(router());

    let (server_stdin_writer, server_stdin) = tokio::io::duplex(4096);
    let (server_stdout, _server_stdout_reader) = tokio::io::duplex(4096);

    let handle = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
    });

    // Close the input side immediately -- the read should return 0 bytes
    // (EOF) and the loop should break and return Ok.
    drop(server_stdin_writer);

    let result = handle.await.expect("transport task join");
    assert!(
        result.is_ok(),
        "run_with_streams must return Ok on EOF, got: {result:?}"
    );
}

// ============================================================================
// BidirectionalStdioTransport
// ============================================================================
//
// The same parse-error coverage as the unidirectional transport, this time
// directly exercising the bug fixed in #797 (bidi closed instead of writing
// a parse-error response). The bidi transport multiplexes stdin / outgoing
// requests / notifications inside a single `tokio::select!`, so a bug in
// any branch could silently drop the loop -- these tests pin the desired
// behavior end-to-end.

#[tokio::test]
async fn bidi_transport_parse_error_wire_shape() {
    let mut transport = BidirectionalStdioTransport::new(router());

    let (server_stdin_writer, server_stdin) = tokio::io::duplex(4096);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(4096);

    let handle = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
    });

    let mut stdin_writer = server_stdin_writer;
    stdin_writer
        .write_all(b"not valid json{{{\n")
        .await
        .unwrap();
    stdin_writer.flush().await.unwrap();
    drop(stdin_writer);

    let reader = BufReader::new(server_stdout_reader);
    let frames = read_n_frames(reader, 1).await;
    handle
        .await
        .expect("transport task join")
        .expect("run_with_streams ok");

    assert_eq!(
        frames.len(),
        1,
        "expected one parse-error frame, got: {frames:?}"
    );
    assert_jsonrpc_error_response(&frames[0]);
    assert!(frames[0]["id"].is_null());
    assert_eq!(frames[0]["error"]["code"].as_i64().unwrap(), -32700);
}

#[tokio::test]
async fn bidi_transport_loop_continues_after_parse_error() {
    let mut transport = BidirectionalStdioTransport::new(router());

    let (server_stdin_writer, server_stdin) = tokio::io::duplex(4096);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(4096);

    let handle = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
    });

    let mut stdin_writer = server_stdin_writer;
    stdin_writer.write_all(b"this is not json\n").await.unwrap();
    stdin_writer
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n")
        .await
        .unwrap();
    stdin_writer.flush().await.unwrap();
    drop(stdin_writer);

    let reader = BufReader::new(server_stdout_reader);
    let frames = read_n_frames(reader, 2).await;
    handle
        .await
        .expect("transport task join")
        .expect("run_with_streams ok");

    assert_eq!(
        frames.len(),
        2,
        "expected parse-error frame + ping response, got: {frames:?}"
    );
    assert_jsonrpc_error_response(&frames[0]);
    assert!(frames[0]["id"].is_null());
    assert_eq!(frames[0]["error"]["code"].as_i64().unwrap(), -32700);

    assert_eq!(frames[1]["jsonrpc"], "2.0");
    assert_eq!(frames[1]["id"], 7);
    assert!(
        frames[1].get("result").is_some(),
        "ping must return a successful result frame, got: {}",
        frames[1]
    );
}

// ============================================================================
// BidirectionalStdioTransport: elicitation / client_requester (#923)
// ============================================================================
//
// #923: BidirectionalStdioTransport::new built a client_requester but never
// attached it to the router, so handler contexts had `client_requester: None`
// and `ctx.can_elicit()` was always false -- elicitation and sampling could
// never work over bidirectional stdio.

#[derive(Debug, Deserialize, JsonSchema)]
struct NoArgs {}

/// Read a single non-empty JSON-RPC frame from `reader`. Panics on EOF; callers
/// wrap this in a timeout so a stalled server surfaces as a test failure rather
/// than a hang.
async fn read_frame<R>(reader: &mut BufReader<R>) -> serde_json::Value
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .expect("read from server output");
        if n == 0 {
            panic!("EOF before a frame was read");
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return serde_json::from_str(trimmed)
            .unwrap_or_else(|e| panic!("invalid JSON on output: {e}: {trimmed}"));
    }
}

/// #923 regression: the handler context must carry a client requester over
/// bidirectional stdio, so `ctx.can_elicit()` is true. A tool reports the value
/// back so the test can assert it end-to-end through the run loop.
#[tokio::test]
async fn bidi_transport_wires_client_requester_into_context() {
    let check = ToolBuilder::new("check_elicit")
        .description("Report whether elicitation is available")
        .extractor_handler((), |ctx: Context, Json(_): Json<NoArgs>| async move {
            Ok(CallToolResult::text(ctx.can_elicit().to_string()))
        })
        .build();

    let router = McpRouter::new()
        .server_info("bidi-elicit-test", "0.0.0")
        .tool(check);
    let mut transport = BidirectionalStdioTransport::new(router);

    let (server_stdin_writer, server_stdin) = tokio::io::duplex(8192);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(8192);

    let handle = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
    });

    let mut w = server_stdin_writer;
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{\"elicitation\":{}},\"clientInfo\":{\"name\":\"t\",\"version\":\"0\"}}}\n").await.unwrap();
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"check_elicit\",\"arguments\":{}}}\n").await.unwrap();
    w.flush().await.unwrap();

    let reader = BufReader::new(server_stdout_reader);
    let frames = read_n_frames(reader, 2).await;
    drop(w);
    let _ = handle.await;

    // Frame 0 = initialize response, frame 1 = tools/call response.
    let call = &frames[1];
    assert_eq!(call["id"], 2, "expected tools/call response, got: {call}");
    let text = call["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert_eq!(
        text, "true",
        "can_elicit() must be true once the requester is wired, got: {call}"
    );
}

/// #923 regression: a full `elicitation/create` round-trip must complete over
/// bidirectional stdio. A tool elicits confirmation; the test acts as the client
/// and answers the server-initiated request. Bounded by a timeout so a run-loop
/// that cannot pump the outgoing request concurrently (deadlock) fails fast
/// instead of hanging.
#[tokio::test]
async fn bidi_transport_elicitation_round_trip() {
    let confirm = ToolBuilder::new("confirm")
        .description("Confirm an action via elicitation")
        .extractor_handler((), |ctx: Context, Json(_): Json<NoArgs>| async move {
            let params = ElicitFormParams {
                message: "Confirm?".to_string(),
                requested_schema: ElicitFormSchema::new().boolean_field(
                    "confirmed",
                    Some("Confirm"),
                    true,
                ),
                mode: None,
                meta: None,
            };
            match ctx.elicit_form(params).await {
                Ok(result) => {
                    let accepted = matches!(result.action, ElicitAction::Accept);
                    Ok(CallToolResult::text(format!("confirmed={accepted}")))
                }
                Err(e) => Ok(CallToolResult::error(format!("elicit failed: {e}"))),
            }
        })
        .build();

    let router = McpRouter::new()
        .server_info("bidi-elicit-test", "0.0.0")
        .tool(confirm);
    let mut transport = BidirectionalStdioTransport::new(router);

    let (server_stdin_writer, server_stdin) = tokio::io::duplex(8192);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(8192);

    let handle = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
    });

    let driver = async move {
        let mut w = server_stdin_writer;
        let mut reader = BufReader::new(server_stdout_reader);

        w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{\"elicitation\":{}},\"clientInfo\":{\"name\":\"t\",\"version\":\"0\"}}}\n").await.unwrap();
        w.flush().await.unwrap();
        let init = read_frame(&mut reader).await;
        assert_eq!(init["id"], 1, "expected initialize response, got: {init}");

        w.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"confirm\",\"arguments\":{}}}\n").await.unwrap();
        w.flush().await.unwrap();

        // Expect the server to initiate elicitation/create, answer it, then
        // receive the tools/call response.
        loop {
            let frame = read_frame(&mut reader).await;
            if frame.get("method").and_then(|m| m.as_str()) == Some("elicitation/create") {
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": frame["id"],
                    "result": { "action": "accept", "content": { "confirmed": true } }
                });
                w.write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
                w.flush().await.unwrap();
                continue;
            }
            if frame["id"] == serde_json::json!(2) {
                assert!(
                    frame.get("result").is_some(),
                    "tools/call must succeed after elicitation, got: {frame}"
                );
                let text = frame["result"]["content"][0]["text"].as_str().unwrap_or("");
                assert_eq!(
                    text, "confirmed=true",
                    "elicitation round-trip should return the client's accept, got: {frame}"
                );
                break;
            }
        }
        drop(w);
    };

    timeout(Duration::from_secs(5), driver)
        .await
        .expect("elicitation round-trip over bidirectional stdio timed out (deadlock)");
    let _ = timeout(Duration::from_secs(2), handle).await;
}
