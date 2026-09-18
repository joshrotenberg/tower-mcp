//! Regression coverage for bidirectional stdio shutdown (#1437).

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};
use tower_mcp::extract::{Context as RequestContext, RawArgs};
use tower_mcp::protocol::{ElicitFormParams, ElicitFormSchema};
use tower_mcp::{BidirectionalStdioTransport, CallToolResult, McpRouter, ToolBuilder};

const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"eof-test","version":"0"}}}"#;
const CALL: &str =
    r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"wait","arguments":{}}}"#;

#[derive(Clone, Default)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl AsyncWrite for CaptureWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl CaptureWriter {
    fn frames(&self) -> Vec<serde_json::Value> {
        let bytes = self.0.lock().unwrap();
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn run_piped_requests(input: String) -> Vec<serde_json::Value> {
    let output = CaptureWriter::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut transport =
            BidirectionalStdioTransport::new(McpRouter::new().server_info("eof-test", "0"));
        timeout(
            Duration::from_secs(2),
            transport.run_with_streams(io::Cursor::new(input), output.clone()),
        )
        .await
        .expect("piped requests must finish")
        .unwrap();
    });
    // Model a main function returning as soon as the transport finishes.
    // Detached tasks must not be needed to finish writing the responses.
    drop(runtime);
    output.frames()
}

#[test]
fn bidi_eof_flushes_a_single_request_before_runtime_shutdown() {
    let frames = run_piped_requests(format!("{INIT}\n"));
    assert_eq!(
        frames.len(),
        1,
        "initialize response lost at EOF: {frames:?}"
    );
    assert_eq!(frames[0]["id"], 1);
    assert!(frames[0]["result"]["capabilities"].is_object());
}

#[test]
fn bidi_eof_flushes_piped_requests_before_runtime_shutdown() {
    let frames = run_piped_requests(format!(
        "{INIT}\n{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}}\n\
         {{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}}\n"
    ));
    assert_eq!(frames.len(), 2, "responses lost at EOF: {frames:?}");
    assert!(
        frames
            .iter()
            .any(|f| f["id"] == 1 && f["result"].is_object())
    );
    assert!(
        frames
            .iter()
            .any(|f| f["id"] == 2 && f["result"]["tools"].is_array())
    );
}

// Both EOF and explicit shutdown must signal the application before waiting
// for handlers. Otherwise an application that releases work during shutdown
// would deadlock against the response drain.
async fn check_waiting_handler(explicit_shutdown: bool, deadline: bool) {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let tool = ToolBuilder::new("wait")
        .description("Wait for the application to release this request")
        .extractor_handler((), {
            let started = started.clone();
            let release = release.clone();
            move |_ctx: RequestContext, RawArgs(_): RawArgs| {
                let started = started.clone();
                let release = release.clone();
                async move {
                    started.notify_one();
                    release.notified().await;
                    Ok(CallToolResult::text("released"))
                }
            }
        })
        .build();
    let mut transport = BidirectionalStdioTransport::new(McpRouter::new().tool(tool));
    if deadline {
        transport = transport.drain_timeout(Duration::from_millis(100));
    }
    // The deadline and stopping handle must also survive middleware wrapping.
    let mut transport = transport.layer(tower::layer::util::Identity::new());
    let handle = transport.handle();
    let requester = transport.client_requester();
    let output = CaptureWriter::default();
    let (mut input, reader) = tokio::io::duplex(4096);
    let writer = output.clone();
    let server = tokio::spawn(async move { transport.run_with_streams(reader, writer).await });
    input
        .write_all(format!("{INIT}\n{CALL}\n").as_bytes())
        .await
        .unwrap();
    timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("handler must start");

    if explicit_shutdown {
        handle.shutdown().unwrap();
    } else {
        input.shutdown().await.unwrap();
    }
    timeout(Duration::from_secs(2), handle.stopping())
        .await
        .expect("stopping must fire before the response drain");
    if !deadline {
        assert!(
            !server.is_finished(),
            "transport returned before its handler"
        );
    }

    assert!(
        timeout(
            Duration::from_secs(2),
            requester.request("ping".into(), serde_json::json!({}))
        )
        .await
        .expect("new client requests must fail while draining")
        .is_err()
    );

    if !deadline {
        release.notify_one();
    }
    timeout(Duration::from_secs(2), server)
        .await
        .expect("drain must finish after release or its deadline")
        .unwrap()
        .unwrap();
    let frames = output.frames();
    if deadline {
        assert!(frames.iter().all(|f| f["id"] != 2), "{frames:?}");
    } else {
        let response = frames.iter().find(|f| f["id"] == 2).expect("tool response");
        assert_eq!(response["result"]["content"][0]["text"], "released");
    }
}

#[tokio::test]
async fn bidi_eof_signals_stopping_before_draining_a_handler() {
    check_waiting_handler(false, false).await;
}

#[tokio::test]
async fn bidi_explicit_shutdown_signals_stopping_before_draining_a_handler() {
    check_waiting_handler(true, false).await;
}

#[tokio::test]
async fn bidi_drain_deadline_survives_layering_and_bounds_a_hung_handler() {
    check_waiting_handler(false, true).await;
}

#[tokio::test]
async fn bidi_eof_releases_a_handler_waiting_for_a_client_response() {
    let tool = ToolBuilder::new("wait")
        .description("Wait for client confirmation")
        .extractor_handler((), |ctx: RequestContext, RawArgs(_): RawArgs| async move {
            let result = ctx
                .elicit_form(ElicitFormParams {
                    message: "Continue?".into(),
                    requested_schema: ElicitFormSchema::new().boolean_field(
                        "confirmed",
                        Some("Confirm"),
                        true,
                    ),
                    mode: None,
                    meta: None,
                })
                .await;
            Ok(match result {
                Ok(_) => CallToolResult::text("confirmed"),
                Err(error) => CallToolResult::error(error.to_string()),
            })
        })
        .build();
    let mut transport = BidirectionalStdioTransport::new(McpRouter::new().tool(tool));
    let requester = transport.client_requester();
    let (mut input, reader) = tokio::io::duplex(4096);
    let (writer, output) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move { transport.run_with_streams(reader, writer).await });
    let mut init: serde_json::Value = serde_json::from_str(INIT).unwrap();
    init["params"]["capabilities"] = serde_json::json!({"elicitation": {}});
    input
        .write_all(format!("{init}\n{CALL}\n").as_bytes())
        .await
        .unwrap();
    let mut output = BufReader::new(output);
    timeout(Duration::from_secs(2), async {
        loop {
            let mut line = String::new();
            assert_ne!(output.read_line(&mut line).await.unwrap(), 0);
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if frame["method"] == "elicitation/create" {
                break;
            }
        }
    })
    .await
    .expect("server must send its elicitation request");
    // The client leaves without answering. The handler must receive an error
    // and finish instead of keeping the default (unbounded) drain open forever.
    drop(input);
    timeout(Duration::from_secs(2), server)
        .await
        .expect("EOF must release pending client requests")
        .unwrap()
        .unwrap();
    let mut line = String::new();
    output.read_line(&mut line).await.unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["id"], 2);
    assert_eq!(response["result"]["isError"], true);

    assert!(
        timeout(
            Duration::from_secs(2),
            requester.request("ping".into(), serde_json::json!({}))
        )
        .await
        .expect("new client requests must fail after shutdown")
        .is_err()
    );
}

#[tokio::test]
async fn bidi_eof_releases_a_client_request_queued_before_run() {
    let mut transport = BidirectionalStdioTransport::new(McpRouter::new());
    let requester = transport.client_requester();
    let mut response = Box::pin(requester.request("ping".into(), serde_json::json!({})));
    assert!(futures::poll!(response.as_mut()).is_pending());

    // Keep the transport alive after run returns: shutdown must close its
    // request channels rather than relying on the transport being dropped.
    transport
        .run_with_streams(tokio::io::empty(), Vec::new())
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_secs(2), response)
            .await
            .expect("queued client request must be released at EOF")
            .is_err()
    );
}
