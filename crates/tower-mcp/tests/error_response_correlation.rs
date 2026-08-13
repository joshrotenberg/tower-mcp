//! An error response has to carry the id of the request that caused it.
//!
//! JSON-RPC 2.0 section 5 permits a null response id only when the id could
//! not be detected, "e.g. Parse error/Invalid Request". A frame that parsed
//! cleanly and then failed semantic validation does not qualify: the id is
//! sitting right there, and a client with more than one request in flight
//! needs it to know which request failed.
//!
//! The stdio and websocket receive loops inspect the raw frame before typing
//! it, and answered those failures with `null` (#1372). The same request sent
//! through `call_single` kept its id, which is why no library-level test saw
//! this. These drive the real loops.

#![cfg(all(feature = "stateless", feature = "http"))]

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28;
use tower_mcp::{CallToolResult, McpRouter, ProtocolSupport, StdioTransport, ToolBuilder};

fn router() -> McpRouter {
    McpRouter::new()
        .server_info("correlation-test", "1.0.0")
        .tool(
            ToolBuilder::new("echo")
                .description("Echo a value")
                .read_only()
                .handler(
                    |v: serde_json::Value| async move { Ok(CallToolResult::text(v.to_string())) },
                )
                .build(),
        )
}

/// Drive one frame through the real stdio loop and read the reply.
async fn over_stdio(frame: serde_json::Value) -> serde_json::Value {
    let mut transport = StdioTransport::new(router())
        .protocol_support(ProtocolSupport::try_new([PROTOCOL_VERSION_2026_07_28]).unwrap());
    let (mut stdin_writer, server_stdin) = tokio::io::duplex(8192);
    let (server_stdout, stdout_reader) = tokio::io::duplex(8192);
    let handle = tokio::spawn(async move {
        let _ = transport
            .run_with_streams(server_stdin, server_stdout)
            .await;
    });

    stdin_writer
        .write_all(format!("{frame}\n").as_bytes())
        .await
        .unwrap();
    drop(stdin_writer);

    let mut line = String::new();
    BufReader::new(stdout_reader)
        .read_line(&mut line)
        .await
        .unwrap();
    let _ = handle.await;
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("reply is not JSON ({e}): {line}"))
}

fn tools_list(id: serde_json::Value, meta: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/list",
        "params": { "_meta": meta }
    })
}

fn supported_meta() -> serde_json::Value {
    serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION_2026_07_28,
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

/// The control. A request the server accepts echoes its id, so a failure in
/// the tests below is about the error path rather than about ids generally.
#[tokio::test]
async fn a_successful_request_echoes_its_id() {
    let response = over_stdio(tools_list(serde_json::json!(42), supported_meta())).await;
    assert_eq!(response["id"], 42);
    assert!(response.get("result").is_some(), "{response}");
}

/// #1372 case B: `-32022`, a protocol version the server does not support.
#[tokio::test]
async fn an_unsupported_protocol_version_error_keeps_the_id() {
    let response = over_stdio(tools_list(
        serde_json::json!(42),
        serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2025-11-25",
            "io.modelcontextprotocol/clientCapabilities": {}
        }),
    ))
    .await;

    assert_eq!(response["error"]["code"], -32022, "{response}");
    assert_eq!(response["id"], 42, "the id is in the frame: {response}");
}

/// #1372 case C: `-32602`, a required `_meta` key missing. A different error
/// path from case B, and it had the same defect.
#[tokio::test]
async fn a_missing_meta_key_error_keeps_the_id() {
    let response = over_stdio(tools_list(
        serde_json::json!(42),
        serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION_2026_07_28
        }),
    ))
    .await;

    assert_eq!(response["error"]["code"], -32602, "{response}");
    assert_eq!(response["id"], 42, "the id is in the frame: {response}");
}

/// JSON-RPC ids are strings or numbers, and a string id has to survive as a
/// string rather than being coerced.
#[tokio::test]
async fn a_string_id_survives_as_a_string() {
    let response = over_stdio(tools_list(
        serde_json::json!("req-abc"),
        serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2025-11-25",
            "io.modelcontextprotocol/clientCapabilities": {}
        }),
    ))
    .await;

    assert_eq!(response["error"]["code"], -32022, "{response}");
    assert_eq!(response["id"], "req-abc", "{response}");
}

/// The boundary this fix deliberately does not cross. A `-32600` says the
/// frame was never a valid request, and this crate refuses every malformed
/// envelope shape uniformly with a null id even when an id is readable
/// (`adversarial_input.rs`). JSON-RPC 2.0 section 5 names Invalid Request in
/// exactly that parenthetical.
#[tokio::test]
async fn a_structurally_invalid_envelope_still_answers_with_a_null_id() {
    let response = over_stdio(serde_json::json!({"jsonrpc": "2.0", "id": 6})).await;

    assert_eq!(response["error"]["code"], -32600, "{response}");
    assert!(
        response["id"].is_null(),
        "a frame that was never a request correlates to nothing: {response}"
    );
}

/// The other half of the rule. A batch has no single id to answer with, so
/// `null` is the honest answer rather than one member's id standing in for
/// the whole rejection.
#[tokio::test]
async fn a_rejected_batch_still_answers_with_a_null_id() {
    let batch = serde_json::json!([
        tools_list(
            serde_json::json!(1),
            serde_json::json!({
                "io.modelcontextprotocol/protocolVersion": "2025-11-25",
                "io.modelcontextprotocol/clientCapabilities": {}
            })
        ),
        tools_list(serde_json::json!(2), supported_meta()),
    ]);
    let response = over_stdio(batch).await;

    // Whatever the server decides about the batch, it must not claim one
    // member's id for a reply covering both.
    if response.get("error").is_some() {
        assert!(
            response["id"].is_null(),
            "a whole-batch rejection cannot borrow a member's id: {response}"
        );
    }
}
