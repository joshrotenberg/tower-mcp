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

    let final_version = tower_mcp::protocol::EXPERIMENTAL_PROTOCOL_VERSION;
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

    assert_eq!(frames[0]["result"]["resultType"], "complete");
    assert_eq!(frames[0]["result"]["ttlMs"], 0);
    assert_eq!(frames[0]["result"]["cacheScope"], "private");
    assert_eq!(frames[1]["result"]["resultType"], "complete");
    assert_eq!(frames[1]["result"]["ttlMs"], 0);
    assert_eq!(
        frames[2]["result"]["content"][0]["text"],
        format!("{final_version}|can_elicit=false")
    );
    assert_eq!(frames[2]["result"]["resultType"], "complete");
    assert!(frames[2]["result"].get("ttlMs").is_none());
    assert_eq!(frames[3]["error"]["code"], -32601);
    assert_eq!(frames[4]["error"]["code"], -32022);
    assert_eq!(frames[4]["error"]["data"]["requested"], "2099-01-01");

    // Legacy responses retain their established wire shape.
    assert!(frames[5]["result"].get("resultType").is_none());
    assert!(frames[6]["result"].get("resultType").is_none());
    assert_eq!(frames[6]["result"]["tools"].as_array().unwrap().len(), 1);
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
            "_meta": final_meta(tower_mcp::protocol::EXPERIMENTAL_PROTOCOL_VERSION)
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
        tower_mcp::protocol::EXPERIMENTAL_PROTOCOL_VERSION
    );
    assert!(
        frames[0]["error"]["data"]["supported"]
            .as_array()
            .unwrap()
            .iter()
            .all(|version| version != tower_mcp::protocol::EXPERIMENTAL_PROTOCOL_VERSION)
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
    let version = tower_mcp::protocol::EXPERIMENTAL_PROTOCOL_VERSION;

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
            "_meta": final_meta(tower_mcp::protocol::EXPERIMENTAL_PROTOCOL_VERSION),
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
        tower_mcp::protocol::EXPERIMENTAL_PROTOCOL_VERSION
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
    let version = tower_mcp::protocol::EXPERIMENTAL_PROTOCOL_VERSION;
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
            "_meta": final_meta(tower_mcp::protocol::EXPERIMENTAL_PROTOCOL_VERSION),
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
            "_meta": final_meta(tower_mcp::protocol::EXPERIMENTAL_PROTOCOL_VERSION),
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
            "_meta": final_meta(tower_mcp::protocol::EXPERIMENTAL_PROTOCOL_VERSION)
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
