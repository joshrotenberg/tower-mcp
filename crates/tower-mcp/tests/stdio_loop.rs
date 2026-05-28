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

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tower_mcp::transport::stdio::BidirectionalStdioTransport;
use tower_mcp::{McpRouter, StdioTransport};
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
