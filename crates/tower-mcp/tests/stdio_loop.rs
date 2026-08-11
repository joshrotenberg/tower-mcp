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
#[cfg(feature = "stateless")]
use tower_mcp::ProtocolSupport;
#[cfg(feature = "stateless")]
use tower_mcp::extract::RawArgs;
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

/// A control or notification branch may win `select!` while stdin holds only
/// part of a JSON frame. The line reader must retain that prefix until the
/// newline arrives instead of discarding it and parsing only the suffix.
#[cfg(feature = "stateless")]
#[tokio::test]
async fn stdio_transport_preserves_partial_frame_when_read_is_cancelled() {
    let mut transport = StdioTransport::new(router());
    let control = transport.handle();
    let (mut server_stdin_writer, server_stdin) = tokio::io::duplex(4096);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(4096);
    let handle = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
    });

    server_stdin_writer
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":42,")
        .await
        .unwrap();
    server_stdin_writer.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    // An unknown subscription is deliberately a no-op, but receiving this
    // control message forces the competing branch to win once.
    control
        .close_subscription(tower_mcp::protocol::RequestId::Number(999))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    server_stdin_writer
        .write_all(b"\"method\":\"ping\"}\n")
        .await
        .unwrap();
    drop(server_stdin_writer);

    let frames = read_n_frames(BufReader::new(server_stdout_reader), 1).await;
    handle
        .await
        .expect("transport task join")
        .expect("run_with_streams ok");
    assert_eq!(frames.len(), 1, "expected one response: {frames:?}");
    assert_eq!(frames[0]["id"], 42, "the request prefix was lost");
    assert!(
        frames[0].get("result").is_some(),
        "unexpected frame: {frames:?}"
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

#[cfg(feature = "stateless")]
fn modern_router() -> McpRouter {
    let inspect = ToolBuilder::new("inspect_meta")
        .description("Report final request metadata visible to the handler")
        .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
            let version = ctx
                .per_request_meta()
                .and_then(|meta| meta.protocol_version.as_deref())
                .unwrap_or("absent");
            Ok(CallToolResult::text(format!(
                "{version}|can_elicit={}",
                ctx.can_elicit()
            )))
        })
        .build();
    McpRouter::new()
        .server_info("stdio-modern-test", "0.0.0")
        .tool(inspect)
}

#[cfg(feature = "stateless")]
fn final_meta(version: &str) -> serde_json::Value {
    serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientInfo": {
            "name": "stdio-loop-client",
            "version": "1.0.0"
        },
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

/// The final stdio lifecycle is selected independently on every request.
/// A modern discovery probe and ordinary calls work before initialize, then
/// legacy initialize traffic can continue on the same byte stream.
#[cfg(feature = "stateless")]
#[tokio::test]
async fn stdio_supports_final_and_legacy_lifecycles_on_one_stream() {
    let mut transport = StdioTransport::new(modern_router());
    let (server_stdin_writer, server_stdin) = tokio::io::duplex(32 * 1024);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(32 * 1024);
    let handle = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
    });

    let final_version = tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28;
    let requests = vec![
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "server/discover",
            "params": {"_meta": final_meta(final_version)}
        }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list",
            "params": {"_meta": final_meta(final_version)}
        }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "inspect_meta",
                "arguments": {},
                "_meta": final_meta(final_version)
            }
        }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 4, "method": "ping",
            "params": {"_meta": final_meta(final_version)}
        }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 5, "method": "server/discover",
            "params": {"_meta": final_meta("2099-01-01")}
        }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 6, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "legacy-client", "version": "1.0.0"}
            }
        }),
        serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/list", "params": {}
        }),
    ];
    let mut stdin_writer = server_stdin_writer;
    for request in requests {
        stdin_writer
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
    }
    stdin_writer.flush().await.unwrap();
    drop(stdin_writer);

    let frames = read_n_frames(BufReader::new(server_stdout_reader), 7).await;
    handle
        .await
        .expect("transport task join")
        .expect("run_with_streams ok");
    assert_eq!(frames.len(), 7, "unexpected frames: {frames:#?}");

    // Requests are handled concurrently (#1231), so responses arrive in
    // completion order. JSON-RPC pairs them by id, and so does this test.
    // The version-rejection error is the exception: it is produced before
    // the id is read, so it goes out with a null id and is matched by code.
    let by_id: std::collections::HashMap<i64, &serde_json::Value> = frames
        .iter()
        .filter_map(|f| f["id"].as_i64().map(|id| (id, f)))
        .collect();
    let response = |id: i64| {
        *by_id
            .get(&id)
            .unwrap_or_else(|| panic!("no response for id {id} in {frames:#?}"))
    };
    let version_error = frames
        .iter()
        .find(|f| f["error"]["code"] == -32022)
        .unwrap_or_else(|| panic!("no version-rejection error in {frames:#?}"));

    assert_eq!(response(1)["result"]["resultType"], "complete");
    assert_eq!(response(1)["result"]["ttlMs"], 0);
    assert_eq!(response(1)["result"]["cacheScope"], "private");
    assert_eq!(response(2)["result"]["resultType"], "complete");
    assert_eq!(response(2)["result"]["ttlMs"], 0);
    assert_eq!(
        response(3)["result"]["content"][0]["text"],
        format!("{final_version}|can_elicit=false")
    );
    assert_eq!(response(3)["result"]["resultType"], "complete");
    assert!(response(3)["result"].get("ttlMs").is_none());
    assert_eq!(response(4)["error"]["code"], -32601);
    assert_eq!(version_error["error"]["data"]["requested"], "2099-01-01");

    // Legacy responses retain their established wire shape.
    assert!(response(6)["result"].get("resultType").is_none());
    assert!(response(7)["result"].get("resultType").is_none());
    assert_eq!(response(7)["result"]["tools"].as_array().unwrap().len(), 1);
}

/// Runtime support can be narrowed even when the final implementation is
/// present in the binary.
#[cfg(feature = "stateless")]
#[tokio::test]
async fn stdio_runtime_protocol_allow_list_is_exact() {
    let mut transport =
        StdioTransport::new(modern_router()).protocol_support(ProtocolSupport::stable());
    let (server_stdin_writer, server_stdin) = tokio::io::duplex(4096);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(4096);
    let handle = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
    });

    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "server/discover",
        "params": {
            "_meta": final_meta(tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28)
        }
    });
    let mut stdin_writer = server_stdin_writer;
    stdin_writer
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    drop(stdin_writer);

    let frames = read_n_frames(BufReader::new(server_stdout_reader), 1).await;
    handle
        .await
        .expect("transport task join")
        .expect("run_with_streams ok");
    assert_eq!(frames[0]["error"]["code"], -32022);
    assert_eq!(
        frames[0]["error"]["data"]["requested"],
        tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28
    );
    assert!(
        frames[0]["error"]["data"]["supported"]
            .as_array()
            .unwrap()
            .iter()
            .all(|version| version != tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28)
    );
}

#[cfg(feature = "stateless")]
fn subscription_router() -> McpRouter {
    let emit = ToolBuilder::new("emit_changes")
        .description("Emit every core subscription-scoped notification")
        .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
            ctx.notify_tools_list_changed();
            ctx.notify_prompts_list_changed();
            ctx.notify_resources_list_changed();
            ctx.notify_resource_updated("file:///watched");
            Ok(CallToolResult::text("emitted"))
        })
        .build();
    McpRouter::new()
        .server_info("stdio-subscription-test", "0.0.0")
        .tool(emit)
}

/// Final subscriptions share stdout, preserve numeric/string IDs verbatim,
/// filter every delivered notification, and distinguish silent client
/// cancellation from graceful server closure.
#[cfg(feature = "stateless")]
#[tokio::test]
async fn stdio_multiplexes_and_gracefully_closes_final_subscriptions() {
    let mut transport = StdioTransport::new(subscription_router());
    let control = transport.handle();
    let (server_stdin_writer, server_stdin) = tokio::io::duplex(32 * 1024);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(32 * 1024);
    let transport_task = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
    });
    let mut writer = server_stdin_writer;
    let mut reader = BufReader::new(server_stdout_reader);
    let version = tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28;

    let tools_listen = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "tools-sub",
        "method": "subscriptions/listen",
        "params": {
            "_meta": final_meta(version),
            "notifications": {
                "toolsListChanged": true,
                "promptsListChanged": false
            }
        }
    });
    writer
        .write_all(format!("{tools_listen}\n").as_bytes())
        .await
        .unwrap();
    writer.flush().await.unwrap();
    let tools_ack = timeout(Duration::from_secs(2), read_frame(&mut reader))
        .await
        .expect("tools subscription acknowledgment");
    assert_eq!(
        tools_ack["method"],
        "notifications/subscriptions/acknowledged"
    );
    assert_eq!(
        tools_ack["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
        "tools-sub"
    );
    assert_eq!(
        tools_ack["params"]["notifications"]["toolsListChanged"],
        true
    );
    assert!(
        tools_ack["params"]["notifications"]
            .get("promptsListChanged")
            .is_none(),
        "false filters must be omitted from the honored filter: {tools_ack}"
    );

    let resources_listen = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 22,
        "method": "subscriptions/listen",
        "params": {
            "_meta": final_meta(version),
            "notifications": {
                "promptsListChanged": true,
                "resourcesListChanged": true,
                "resourceSubscriptions": ["file:///watched"]
            }
        }
    });
    writer
        .write_all(format!("{resources_listen}\n").as_bytes())
        .await
        .unwrap();
    writer.flush().await.unwrap();
    let resources_ack = timeout(Duration::from_secs(2), read_frame(&mut reader))
        .await
        .expect("resources subscription acknowledgment");
    assert_eq!(
        resources_ack["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
        22
    );

    let emit = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "emit_changes",
            "arguments": {},
            "_meta": final_meta(version)
        }
    });
    writer
        .write_all(format!("{emit}\n").as_bytes())
        .await
        .unwrap();
    writer.flush().await.unwrap();

    let mut first_delivery = Vec::new();
    for _ in 0..5 {
        first_delivery.push(
            timeout(Duration::from_secs(2), read_frame(&mut reader))
                .await
                .expect("first subscription delivery"),
        );
    }
    assert!(first_delivery.iter().any(|frame| frame["id"] == 3));
    let delivered: Vec<_> = first_delivery
        .iter()
        .filter(|frame| frame.get("method").is_some())
        .collect();
    assert_eq!(
        delivered.len(),
        4,
        "unexpected delivery: {first_delivery:#?}"
    );
    for frame in delivered {
        let method = frame["method"].as_str().unwrap();
        let id = &frame["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"];
        match method {
            "notifications/tools/list_changed" => assert_eq!(id, "tools-sub"),
            "notifications/prompts/list_changed"
            | "notifications/resources/list_changed"
            | "notifications/resources/updated" => assert_eq!(id, 22),
            _ => panic!("unexpected subscription notification: {frame}"),
        }
    }

    // Client cancellation is silent and removes only the named subscription.
    let cancel = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {"requestId": "tools-sub"}
    });
    let emit_again = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "emit_changes",
            "arguments": {},
            "_meta": final_meta(version)
        }
    });
    writer
        .write_all(format!("{cancel}\n{emit_again}\n").as_bytes())
        .await
        .unwrap();
    writer.flush().await.unwrap();

    let mut after_cancel = Vec::new();
    for _ in 0..4 {
        after_cancel.push(
            timeout(Duration::from_secs(2), read_frame(&mut reader))
                .await
                .expect("delivery after cancellation"),
        );
    }
    assert!(after_cancel.iter().any(|frame| frame["id"] == 4));
    assert!(
        after_cancel
            .iter()
            .filter(|frame| frame.get("method").is_some())
            .all(|frame| {
                frame["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"]
                    == serde_json::json!(22)
            })
    );

    // Server closure returns the complete result but leaves stdout usable.
    control
        .close_subscription(tower_mcp::RequestId::Number(22))
        .unwrap();
    let complete = timeout(Duration::from_secs(2), read_frame(&mut reader))
        .await
        .expect("graceful subscription result");
    assert_eq!(complete["id"], 22);
    assert_eq!(complete["result"]["resultType"], "complete");
    assert_eq!(
        complete["result"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
        22
    );
    assert_eq!(
        complete["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "stdio-subscription-test"
    );

    let list = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/list",
        "params": {"_meta": final_meta(version)}
    });
    writer
        .write_all(format!("{list}\n").as_bytes())
        .await
        .unwrap();
    writer.flush().await.unwrap();
    let list_response = timeout(Duration::from_secs(2), read_frame(&mut reader))
        .await
        .expect("shared channel remains usable");
    assert_eq!(list_response["id"], 5);

    // Whole-server shutdown drains every remaining subscription result.
    let shutdown_listen = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "shutdown-sub",
        "method": "subscriptions/listen",
        "params": {
            "_meta": final_meta(version),
            "notifications": {"toolsListChanged": true}
        }
    });
    writer
        .write_all(format!("{shutdown_listen}\n").as_bytes())
        .await
        .unwrap();
    writer.flush().await.unwrap();
    let shutdown_ack = timeout(Duration::from_secs(2), read_frame(&mut reader))
        .await
        .expect("shutdown subscription acknowledgment");
    assert_eq!(
        shutdown_ack["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
        "shutdown-sub"
    );
    control.shutdown().unwrap();
    let shutdown_result = timeout(Duration::from_secs(2), read_frame(&mut reader))
        .await
        .expect("shutdown drains subscription result");
    assert_eq!(shutdown_result["id"], "shutdown-sub");
    assert_eq!(shutdown_result["result"]["resultType"], "complete");

    transport_task
        .await
        .expect("transport task join")
        .expect("run_with_streams ok");
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn stdio_subscription_validation_uses_runtime_protocol_policy() {
    let mut transport =
        StdioTransport::new(subscription_router()).protocol_support(ProtocolSupport::stable());
    let (mut server_stdin_writer, server_stdin) = tokio::io::duplex(8192);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(8192);
    let handle = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
    });
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "subscriptions/listen",
        "params": {
            "_meta": final_meta(tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28),
            "notifications": {"toolsListChanged": true}
        }
    });
    server_stdin_writer
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    drop(server_stdin_writer);

    let frames = read_n_frames(BufReader::new(server_stdout_reader), 1).await;
    handle
        .await
        .expect("transport task join")
        .expect("run_with_streams ok");
    assert_eq!(frames[0]["id"], 9);
    assert_eq!(frames[0]["error"]["code"], -32022);
    assert_eq!(
        frames[0]["error"]["data"]["requested"],
        tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28
    );
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn stdio_subscription_requires_final_metadata_and_filter() {
    let mut transport = StdioTransport::new(subscription_router());
    let (mut server_stdin_writer, server_stdin) = tokio::io::duplex(8192);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(8192);
    let handle = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
    });
    let version = tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28;
    let missing_filter = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "subscriptions/listen",
        "params": {"_meta": final_meta(version)}
    });
    let missing_capabilities = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "subscriptions/listen",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": version
            },
            "notifications": {"toolsListChanged": true}
        }
    });
    let legacy_shape = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 12,
        "method": "subscriptions/listen",
        "params": {"notifications": {"toolsListChanged": true}}
    });
    server_stdin_writer
        .write_all(format!("{missing_filter}\n{missing_capabilities}\n{legacy_shape}\n").as_bytes())
        .await
        .unwrap();
    drop(server_stdin_writer);

    let frames = read_n_frames(BufReader::new(server_stdout_reader), 3).await;
    handle
        .await
        .expect("transport task join")
        .expect("run_with_streams ok");
    assert_eq!(frames[0]["id"], 10);
    assert_eq!(frames[0]["error"]["code"], -32602);
    assert_eq!(frames[1]["id"], 11);
    assert_eq!(frames[1]["error"]["code"], -32602);
    assert_eq!(frames[2]["id"], 12);
    assert_eq!(
        frames[2]["error"]["code"], -32600,
        "claimless listen must remain on the legacy lifecycle and fail before initialize"
    );
    assert!(
        frames.iter().all(|frame| frame.get("method").is_none()),
        "invalid listens must not be acknowledged: {frames:#?}"
    );
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn middleware_wrapped_stdio_preserves_subscription_routing() {
    let mut transport =
        StdioTransport::new(subscription_router()).layer(tower::layer::util::Identity::new());
    let control = transport.handle();
    let (mut server_stdin_writer, server_stdin) = tokio::io::duplex(8192);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(8192);
    let task = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
    });
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "layered",
        "method": "subscriptions/listen",
        "params": {
            "_meta": final_meta(tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28),
            "notifications": {"toolsListChanged": true}
        }
    });
    server_stdin_writer
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    server_stdin_writer.flush().await.unwrap();
    let mut reader = BufReader::new(server_stdout_reader);
    let ack = timeout(Duration::from_secs(2), read_frame(&mut reader))
        .await
        .expect("layered subscription acknowledgment");
    assert_eq!(
        ack["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
        "layered"
    );

    control
        .close_subscription(tower_mcp::RequestId::String("layered".to_string()))
        .unwrap();
    let complete = timeout(Duration::from_secs(2), read_frame(&mut reader))
        .await
        .expect("layered graceful result");
    assert_eq!(complete["id"], "layered");
    assert_eq!(complete["result"]["resultType"], "complete");
    assert!(
        complete["result"]["_meta"]
            .get("io.modelcontextprotocol/serverInfo")
            .is_none(),
        "generic services do not expose server identity to the transport"
    );
    control.shutdown().unwrap();
    task.await
        .expect("transport task join")
        .expect("run_with_streams ok");
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn bidi_stdio_preserves_subscription_routing() {
    let mut transport = BidirectionalStdioTransport::new(subscription_router());
    let control = transport.handle();
    let (mut server_stdin_writer, server_stdin) = tokio::io::duplex(8192);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(8192);
    let task = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
    });
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 71,
        "method": "subscriptions/listen",
        "params": {
            "_meta": final_meta(tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28),
            "notifications": {"toolsListChanged": true}
        }
    });
    server_stdin_writer
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    server_stdin_writer.flush().await.unwrap();
    let mut reader = BufReader::new(server_stdout_reader);
    let ack = timeout(Duration::from_secs(2), read_frame(&mut reader))
        .await
        .expect("bidirectional subscription acknowledgment");
    assert_eq!(
        ack["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
        71
    );

    control
        .close_subscription(tower_mcp::RequestId::Number(71))
        .unwrap();
    let complete = timeout(Duration::from_secs(2), read_frame(&mut reader))
        .await
        .expect("bidirectional graceful result");
    assert_eq!(complete["id"], 71);
    assert_eq!(complete["result"]["resultType"], "complete");
    assert_eq!(
        complete["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "stdio-subscription-test"
    );
    control.shutdown().unwrap();
    task.await
        .expect("transport task join")
        .expect("run_with_streams ok");
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn bidi_final_handlers_cannot_initiate_client_requests() {
    let mut transport = BidirectionalStdioTransport::new(modern_router());
    let (server_stdin_writer, server_stdin) = tokio::io::duplex(8192);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(8192);
    let handle = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
    });

    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "inspect_meta",
            "arguments": {},
            "_meta": final_meta(tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28)
        }
    });
    let mut stdin_writer = server_stdin_writer;
    stdin_writer
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    stdin_writer.flush().await.unwrap();

    let mut reader = BufReader::new(server_stdout_reader);
    let frame = timeout(Duration::from_secs(2), read_frame(&mut reader))
        .await
        .expect("final tool response");
    assert_eq!(
        frame["result"]["content"][0]["text"],
        "2026-07-28|can_elicit=false"
    );
    assert_eq!(frame["result"]["resultType"], "complete");

    drop(stdin_writer);
    handle
        .await
        .expect("transport task join")
        .expect("run_with_streams ok");
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

// =============================================================================
// Middleware observation of subscriptions/listen (#1182)
// =============================================================================

#[cfg(feature = "stateless")]
mod listen_observation {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::task::{Context as TaskContext, Poll};

    use tower_mcp::router::{RouterRequest, RouterResponse};

    #[derive(Clone)]
    struct RecordingLayer {
        seen: Arc<Mutex<Vec<String>>>,
    }

    #[derive(Clone)]
    struct RecordingService<S> {
        inner: S,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl<S> tower::Layer<S> for RecordingLayer {
        type Service = RecordingService<S>;

        fn layer(&self, inner: S) -> Self::Service {
            RecordingService {
                inner,
                seen: self.seen.clone(),
            }
        }
    }

    impl<S> tower_service::Service<RouterRequest> for RecordingService<S>
    where
        S: tower_service::Service<RouterRequest, Response = RouterResponse>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = S::Future;

        fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        fn call(&mut self, request: RouterRequest) -> Self::Future {
            self.seen
                .lock()
                .unwrap()
                .push(request.inner.method_name().to_string());
            self.inner.call(request)
        }
    }

    async fn run_listen_exchange(request: serde_json::Value) -> (Vec<String>, serde_json::Value) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut transport =
            StdioTransport::new(subscription_router()).layer(RecordingLayer { seen: seen.clone() });
        let control = transport.handle();
        let (mut server_stdin_writer, server_stdin) = tokio::io::duplex(8192);
        let (server_stdout, server_stdout_reader) = tokio::io::duplex(8192);
        let task = tokio::spawn(async move {
            transport
                .run_with_streams(server_stdin, server_stdout)
                .await
        });
        server_stdin_writer
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        server_stdin_writer.flush().await.unwrap();
        let mut reader = BufReader::new(server_stdout_reader);
        let frame = timeout(Duration::from_secs(2), read_frame(&mut reader))
            .await
            .expect("listen exchange reply");
        control.shutdown().unwrap();
        let _ = task.await;
        let observed = seen.lock().unwrap().clone();
        (observed, frame)
    }

    /// Before #1182 the transport intercepted `subscriptions/listen` ahead of
    /// the service, so a layer advertised as covering all MCP requests never
    /// saw subscription starts. Now the accepted listen passes through it.
    #[tokio::test]
    async fn middleware_observes_an_accepted_listen() {
        let (observed, frame) = run_listen_exchange(serde_json::json!({
            "jsonrpc": "2.0",
            "id": "observed",
            "method": "subscriptions/listen",
            "params": {
                "_meta": final_meta(tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28),
                "notifications": {"toolsListChanged": true}
            }
        }))
        .await;

        assert_eq!(observed, vec!["subscriptions/listen"]);
        // The wire behavior is unchanged: the acknowledgment still arrives.
        assert_eq!(frame["method"], "notifications/subscriptions/acknowledged");
        assert_eq!(
            frame["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
            "observed"
        );
        assert_eq!(frame["params"]["notifications"]["toolsListChanged"], true);
    }

    /// A rejected listen also passes through the layer, and the rejection
    /// wire shape matches what the transport-owned validation produced
    /// before: -32021 with the required extension named.
    #[tokio::test]
    async fn middleware_observes_a_rejected_listen() {
        let (observed, frame) = run_listen_exchange(serde_json::json!({
            "jsonrpc": "2.0",
            "id": "rejected",
            "method": "subscriptions/listen",
            "params": {
                "_meta": final_meta(tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28),
                "notifications": {"taskIds": ["task-1"]}
            }
        }))
        .await;

        assert_eq!(observed, vec!["subscriptions/listen"]);
        assert_eq!(frame["id"], "rejected");
        assert_eq!(frame["error"]["code"], -32021);
        assert!(
            frame["error"]["data"]["requiredCapabilities"]["extensions"]
                ["io.modelcontextprotocol/tasks"]
                .is_object(),
            "the rejection must name the missing extension: {frame}"
        );
    }

    /// A listen violating the wire schema (the required `notifications`
    /// field missing) is rejected by protocol inspection inside the service,
    /// before the middleware boundary. That matches every other method:
    /// schema-invalid requests never become a `RouterRequest`, so middleware
    /// does not observe them; semantic rejections (like -32021 above) do.
    #[tokio::test]
    async fn schema_rejections_stay_ahead_of_the_middleware_boundary() {
        let (observed, frame) = run_listen_exchange(serde_json::json!({
            "jsonrpc": "2.0",
            "id": "no-filter",
            "method": "subscriptions/listen",
            "params": {
                "_meta": final_meta(tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28)
            }
        }))
        .await;

        assert!(observed.is_empty(), "schema rejections precede middleware");
        assert_eq!(frame["id"], "no-filter");
        assert_eq!(frame["error"]["code"], -32602);
        assert!(
            frame["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("required `notifications` field is missing"),
            "the rejection must explain the missing filter: {frame}"
        );
    }
}

// =============================================================================
// Close observation (#1182, terminal half)
// =============================================================================

#[cfg(feature = "stateless")]
mod close_observation {
    use super::*;
    use std::sync::{Arc, Mutex};

    use tower_mcp::{SubscriptionClose, SubscriptionCloseReason, SubscriptionObserver};

    #[derive(Default)]
    struct RecordingObserver {
        closes: Mutex<Vec<SubscriptionClose>>,
    }

    impl SubscriptionObserver for RecordingObserver {
        fn on_close(&self, close: SubscriptionClose) {
            self.closes.lock().unwrap().push(close);
        }
    }

    fn observed_router(observer: Arc<RecordingObserver>) -> McpRouter {
        subscription_router().with_subscription_observer(observer)
    }

    fn listen_frame(id: &str) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "subscriptions/listen",
            "params": {
                "_meta": final_meta(tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28),
                "notifications": {"toolsListChanged": true}
            }
        })
    }

    /// The client cancels its subscription; the observer records the reason
    /// and a duration measured from acceptance.
    #[tokio::test]
    async fn cancellation_reaches_the_observer() {
        let observer = Arc::new(RecordingObserver::default());
        let mut transport = StdioTransport::new(observed_router(observer.clone()));
        let control = transport.handle();
        let (mut stdin_writer, server_stdin) = tokio::io::duplex(8192);
        let (server_stdout, stdout_reader) = tokio::io::duplex(8192);
        let task = tokio::spawn(async move {
            transport
                .run_with_streams(server_stdin, server_stdout)
                .await
        });

        stdin_writer
            .write_all(format!("{}\n", listen_frame("cancel-me")).as_bytes())
            .await
            .unwrap();
        let mut reader = BufReader::new(stdout_reader);
        let _ack = timeout(Duration::from_secs(2), read_frame(&mut reader))
            .await
            .expect("acknowledgment");

        let cancel = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": "cancel-me"}
        });
        stdin_writer
            .write_all(format!("{cancel}\n").as_bytes())
            .await
            .unwrap();
        stdin_writer.flush().await.unwrap();

        // The cancellation produces no frame; give the loop a beat, then
        // shut down and join before asserting.
        tokio::time::sleep(Duration::from_millis(50)).await;
        control.shutdown().unwrap();
        let _ = task.await;

        let closes = observer.closes.lock().unwrap();
        assert_eq!(closes.len(), 1, "exactly one close record");
        assert_eq!(
            closes[0].subscription_id,
            tower_mcp::RequestId::String("cancel-me".to_string())
        );
        assert_eq!(closes[0].reason, SubscriptionCloseReason::Cancelled);
    }

    /// A server-driven graceful close reports Drained; the terminal frame
    /// still reaches the wire.
    #[tokio::test]
    async fn graceful_close_reports_drained() {
        let observer = Arc::new(RecordingObserver::default());
        let mut transport = StdioTransport::new(observed_router(observer.clone()));
        let control = transport.handle();
        let (mut stdin_writer, server_stdin) = tokio::io::duplex(8192);
        let (server_stdout, stdout_reader) = tokio::io::duplex(8192);
        let task = tokio::spawn(async move {
            transport
                .run_with_streams(server_stdin, server_stdout)
                .await
        });

        stdin_writer
            .write_all(format!("{}\n", listen_frame("drain-me")).as_bytes())
            .await
            .unwrap();
        stdin_writer.flush().await.unwrap();
        let mut reader = BufReader::new(stdout_reader);
        let _ack = timeout(Duration::from_secs(2), read_frame(&mut reader))
            .await
            .expect("acknowledgment");

        control
            .close_subscription(tower_mcp::RequestId::String("drain-me".to_string()))
            .unwrap();
        let complete = timeout(Duration::from_secs(2), read_frame(&mut reader))
            .await
            .expect("graceful terminal result");
        assert_eq!(complete["result"]["resultType"], "complete");

        control.shutdown().unwrap();
        let _ = task.await;

        let closes = observer.closes.lock().unwrap();
        assert_eq!(closes.len(), 1);
        assert_eq!(closes[0].reason, SubscriptionCloseReason::Drained);
    }

    /// EOF with a live stream reports Disconnected: the connection died and
    /// no terminal frame could be written.
    #[tokio::test]
    async fn eof_reports_disconnected() {
        let observer = Arc::new(RecordingObserver::default());
        let mut transport = StdioTransport::new(observed_router(observer.clone()));
        let (mut stdin_writer, server_stdin) = tokio::io::duplex(8192);
        let (server_stdout, stdout_reader) = tokio::io::duplex(8192);
        let task = tokio::spawn(async move {
            transport
                .run_with_streams(server_stdin, server_stdout)
                .await
        });

        stdin_writer
            .write_all(format!("{}\n", listen_frame("drop-me")).as_bytes())
            .await
            .unwrap();
        stdin_writer.flush().await.unwrap();
        let mut reader = BufReader::new(stdout_reader);
        let _ack = timeout(Duration::from_secs(2), read_frame(&mut reader))
            .await
            .expect("acknowledgment");

        // Closing the write half is EOF for the transport's read loop.
        drop(stdin_writer);
        timeout(Duration::from_secs(2), task)
            .await
            .expect("loop must end at EOF")
            .expect("join")
            .expect("run ok");

        let closes = observer.closes.lock().unwrap();
        assert_eq!(closes.len(), 1);
        assert_eq!(
            closes[0].subscription_id,
            tower_mcp::RequestId::String("drop-me".to_string())
        );
        assert_eq!(closes[0].reason, SubscriptionCloseReason::Disconnected);
    }
}

// ============================================================================
// Concurrent request handling (#1231)
// ============================================================================

mod concurrency {
    use super::*;
    use tower_mcp::extract::RawArgs;

    /// `slow` sleeps long enough that a serial loop cannot possibly answer
    /// `fast` first, so ordering alone tells us which behaviour we got.
    const SLOW: Duration = Duration::from_millis(400);

    fn router_with_a_slow_tool() -> McpRouter {
        let slow = ToolBuilder::new("slow")
            .description("Sleeps before answering")
            .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                tokio::time::sleep(SLOW).await;
                Ok(CallToolResult::text("slow"))
            })
            .build();
        let fast = ToolBuilder::new("fast")
            .description("Answers immediately")
            .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                Ok(CallToolResult::text("fast"))
            })
            .build();
        McpRouter::new()
            .server_info("stdio-concurrency-test", "0.0.0")
            .tool(slow)
            .tool(fast)
    }

    const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    const CALL_SLOW: &str =
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"slow","arguments":{}}}"#;
    const CALL_FAST: &str =
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"fast","arguments":{}}}"#;

    /// Drive a transport with the handshake plus both tool calls, then close
    /// stdin. Returns the response ids in the order they were written.
    async fn response_id_order(mut transport: StdioTransport) -> Vec<i64> {
        let (mut stdin_writer, server_stdin) = tokio::io::duplex(4096);
        let (server_stdout, server_stdout_reader) = tokio::io::duplex(4096);

        let handle = tokio::spawn(async move {
            transport
                .run_with_streams(server_stdin, server_stdout)
                .await
        });

        for line in [INIT, CALL_SLOW, CALL_FAST] {
            stdin_writer.write_all(line.as_bytes()).await.unwrap();
            stdin_writer.write_all(b"\n").await.unwrap();
        }
        stdin_writer.flush().await.unwrap();
        // EOF while both calls are still in flight: their responses must
        // still be written before the loop returns.
        drop(stdin_writer);

        let frames = timeout(
            Duration::from_secs(10),
            read_n_frames(BufReader::new(server_stdout_reader), 3),
        )
        .await
        .expect("responses must arrive");

        handle
            .await
            .expect("transport task join")
            .expect("run_with_streams ok");

        assert_eq!(frames.len(), 3, "expected three responses, got {frames:?}");
        frames
            .iter()
            .map(|f| f["id"].as_i64().expect("response carries an id"))
            .collect()
    }

    /// #1231: the loop awaited each handler inline, so one slow tool blocked
    /// every other call on the connection. The fast call is issued second and
    /// must still be answered first.
    #[tokio::test]
    async fn a_slow_tool_does_not_block_later_requests() {
        let ids = response_id_order(StdioTransport::new(router_with_a_slow_tool())).await;
        assert_eq!(
            ids,
            vec![1, 3, 2],
            "the fast call (3) was issued after the slow one (2) and must be answered first"
        );
    }

    /// `max_concurrent_requests(1)` is the escape hatch back to the old
    /// behaviour for handlers that assume no two requests overlap.
    #[tokio::test]
    async fn a_limit_of_one_restores_serial_handling() {
        let transport = StdioTransport::new(router_with_a_slow_tool()).max_concurrent_requests(1);
        let ids = response_id_order(transport).await;
        assert_eq!(
            ids,
            vec![1, 2, 3],
            "with a limit of 1 the slow call (2) must be answered before the fast one (3)"
        );
    }
}

// ============================================================================
// Control traffic under a concurrency cap (#1251)
// ============================================================================

mod control_under_saturation {
    use super::*;
    use tower_mcp::extract::RawArgs;

    /// A tool that only finishes when its request is cancelled.
    fn router_with_a_waiting_tool() -> McpRouter {
        let wait = ToolBuilder::new("wait")
            .description("Waits until cancelled")
            .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
                ctx.cancelled().await;
                Ok(CallToolResult::text("cancelled"))
            })
            .build();
        McpRouter::new()
            .server_info("stdio-control-test", "0.0.0")
            .tool(wait)
    }

    const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    const CALL_WAIT: &str =
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"wait","arguments":{}}}"#;
    const PING: &str = r#"{"jsonrpc":"2.0","id":3,"method":"ping","params":{}}"#;
    const CANCEL: &str =
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}}"#;

    /// Control: with no cap at all, does stdio cancellation reach a running
    /// handler? If this fails the problem is the harness, not the limit.
    #[tokio::test]
    async fn cancellation_reaches_a_running_handler_without_a_limit() {
        let mut transport = StdioTransport::new(router_with_a_waiting_tool());

        let (mut stdin_writer, server_stdin) = tokio::io::duplex(4096);
        let (server_stdout, server_stdout_reader) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            transport
                .run_with_streams(server_stdin, server_stdout)
                .await
        });

        for line in [INIT, CALL_WAIT] {
            stdin_writer.write_all(line.as_bytes()).await.unwrap();
            stdin_writer.write_all(b"\n").await.unwrap();
        }
        stdin_writer.flush().await.unwrap();
        // Let the call register as in-flight first. A real client cancels
        // something it has observed; a cancellation that overtakes its own
        // request names an id the server has not seen yet.
        tokio::time::sleep(Duration::from_millis(150)).await;
        for line in [PING, CANCEL] {
            stdin_writer.write_all(line.as_bytes()).await.unwrap();
            stdin_writer.write_all(b"\n").await.unwrap();
        }
        stdin_writer.flush().await.unwrap();

        let frames = timeout(
            Duration::from_secs(5),
            read_n_frames(BufReader::new(server_stdout_reader), 3),
        )
        .await
        .expect("unlimited transport must answer all three");
        let ids: Vec<i64> = frames.iter().filter_map(|f| f["id"].as_i64()).collect();
        assert!(ids.contains(&2), "cancelled call answered: {frames:?}");
    }

    /// #1251: the permit is acquired inside the read loop, so once every slot
    /// is busy the loop stops reading. A `notifications/cancelled` queued
    /// behind an ordinary request can then never be read, and the request it
    /// would have cancelled never releases its permit. Nothing progresses.
    ///
    /// The transport's own documentation says cancellation has to overtake
    /// the request it cancels, which is exactly what a cap prevents today.
    #[tokio::test]
    async fn a_saturated_limit_must_not_starve_cancellation() {
        let mut transport =
            StdioTransport::new(router_with_a_waiting_tool()).max_concurrent_requests(1);

        let (mut stdin_writer, server_stdin) = tokio::io::duplex(4096);
        let (server_stdout, server_stdout_reader) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            transport
                .run_with_streams(server_stdin, server_stdout)
                .await
        });

        for line in [INIT, CALL_WAIT] {
            stdin_writer.write_all(line.as_bytes()).await.unwrap();
            stdin_writer.write_all(b"\n").await.unwrap();
        }
        stdin_writer.flush().await.unwrap();
        // Let the call register as in-flight first. A real client cancels
        // something it has observed; a cancellation that overtakes its own
        // request names an id the server has not seen yet.
        tokio::time::sleep(Duration::from_millis(150)).await;
        for line in [PING, CANCEL] {
            stdin_writer.write_all(line.as_bytes()).await.unwrap();
            stdin_writer.write_all(b"\n").await.unwrap();
        }
        stdin_writer.flush().await.unwrap();

        let frames = timeout(
            Duration::from_secs(5),
            read_n_frames(BufReader::new(server_stdout_reader), 3),
        )
        .await
        .expect("cancellation must be readable while the limit is saturated");

        let ids: Vec<i64> = frames.iter().filter_map(|f| f["id"].as_i64()).collect();
        assert!(
            ids.contains(&2),
            "the cancelled call must be answered: {frames:?}"
        );
        assert!(
            ids.contains(&3),
            "the queued request must run once the permit frees: {frames:?}"
        );
    }
}
