//! The same core behaviours, asserted across every stdio configuration.
//!
//! #1258: our tests covered one request, on the default configuration, doing
//! something reasonable. Two real bugs lived exactly one configuration over:
//! applying a tower layer silently stopped `notifications/cancelled` reaching
//! a handler (#1250), and a saturated concurrency cap stopped the transport
//! reading at all (#1251). Both were found in the field rather than here.
//!
//! Anything asserted in this file is asserted for all of them, so a
//! configuration-specific regression fails rather than hiding.

use std::future::Future;
use std::pin::Pin;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::time::{Duration, timeout};
use tower_mcp::extract::{Context, RawArgs};
use tower_mcp::{CallToolResult, McpRouter, StdioTransport, ToolBuilder};

type ServerFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type Serve = fn(McpRouter, DuplexStream, DuplexStream) -> ServerFuture;

/// Every stdio configuration a server can reasonably be run in.
fn configurations() -> Vec<(&'static str, Serve)> {
    fn plain(router: McpRouter, stdin: DuplexStream, stdout: DuplexStream) -> ServerFuture {
        Box::pin(async move {
            let _ = StdioTransport::new(router)
                .run_with_streams(stdin, stdout)
                .await;
        })
    }
    fn layered(router: McpRouter, stdin: DuplexStream, stdout: DuplexStream) -> ServerFuture {
        Box::pin(async move {
            let _ = StdioTransport::new(router)
                .layer(tower::layer::util::Identity::new())
                .run_with_streams(stdin, stdout)
                .await;
        })
    }
    fn silent(router: McpRouter, stdin: DuplexStream, stdout: DuplexStream) -> ServerFuture {
        Box::pin(async move {
            let _ = StdioTransport::without_server_notifications(router)
                .run_with_streams(stdin, stdout)
                .await;
        })
    }
    fn serial(router: McpRouter, stdin: DuplexStream, stdout: DuplexStream) -> ServerFuture {
        Box::pin(async move {
            let _ = StdioTransport::new(router)
                .max_concurrent_requests(1)
                .run_with_streams(stdin, stdout)
                .await;
        })
    }
    fn capped(router: McpRouter, stdin: DuplexStream, stdout: DuplexStream) -> ServerFuture {
        Box::pin(async move {
            let _ = StdioTransport::new(router)
                .max_concurrent_requests(4)
                .run_with_streams(stdin, stdout)
                .await;
        })
    }
    fn bounded_drain(router: McpRouter, stdin: DuplexStream, stdout: DuplexStream) -> ServerFuture {
        Box::pin(async move {
            let _ = StdioTransport::new(router)
                .drain_timeout(Duration::from_secs(5))
                .run_with_streams(stdin, stdout)
                .await;
        })
    }

    vec![
        ("plain", plain as Serve),
        ("layered", layered as Serve),
        ("without_server_notifications", silent as Serve),
        ("max_concurrent_requests(1)", serial as Serve),
        ("max_concurrent_requests(4)", capped as Serve),
        ("drain_timeout", bounded_drain as Serve),
    ]
}

fn router() -> McpRouter {
    let echo = ToolBuilder::new("echo")
        .description("Answers immediately")
        .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
            Ok(CallToolResult::text("echo"))
        })
        .build();
    let wait = ToolBuilder::new("wait")
        .description("Waits until cancelled")
        .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
            ctx.cancelled().await;
            Ok(CallToolResult::text("cancelled"))
        })
        .build();
    let slow = ToolBuilder::new("slow")
        .description("Takes a moment")
        .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            Ok(CallToolResult::text("slow"))
        })
        .build();
    McpRouter::new()
        .server_info("matrix", "0.0.0")
        .tool(echo)
        .tool(wait)
        .tool(slow)
}

const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

fn call(id: u32, tool: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{tool}","arguments":{{}}}}}}"#
    )
}

async fn read_n_frames<R>(mut reader: BufReader<R>, expected: usize) -> Vec<serde_json::Value>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut out = Vec::with_capacity(expected);
    while out.len() < expected {
        let mut line = String::new();
        if reader.read_line(&mut line).await.expect("read") == 0 {
            break;
        }
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            out.push(serde_json::from_str(trimmed).expect("valid JSON frame"));
        }
    }
    out
}

/// Drive one configuration: write `before`, pause so the server registers
/// them, write `after`, then read `expected` frames.
async fn exchange(
    serve: Serve,
    before: &[String],
    after: &[String],
    expected: usize,
) -> Vec<serde_json::Value> {
    let (mut writer, server_stdin) = tokio::io::duplex(8192);
    let (server_stdout, reader) = tokio::io::duplex(8192);
    tokio::spawn(serve(router(), server_stdin, server_stdout));

    for line in before {
        writer.write_all(line.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
    }
    writer.flush().await.unwrap();

    if !after.is_empty() {
        // A cancellation naming a request the server has not registered yet
        // is dropped, which is a client ordering concern rather than
        // anything about the configuration under test.
        tokio::time::sleep(Duration::from_millis(150)).await;
        for line in after {
            writer.write_all(line.as_bytes()).await.unwrap();
            writer.write_all(b"\n").await.unwrap();
        }
        writer.flush().await.unwrap();
    }

    timeout(
        Duration::from_secs(10),
        read_n_frames(BufReader::new(reader), expected),
    )
    .await
    .expect("frames must arrive")
}

fn id_of(frame: &serde_json::Value) -> Option<i64> {
    frame["id"].as_i64()
}

// ============================================================================
// The shared assertions
// ============================================================================

#[tokio::test]
async fn every_configuration_answers_a_request() {
    for (name, serve) in configurations() {
        let frames = exchange(
            serve,
            &[INIT.to_string(), INITIALIZED.to_string(), call(2, "echo")],
            &[],
            2,
        )
        .await;
        let answer = frames
            .iter()
            .find(|f| id_of(f) == Some(2))
            .unwrap_or_else(|| panic!("[{name}] no answer: {frames:?}"));
        assert_eq!(
            answer["result"]["content"][0]["text"], "echo",
            "[{name}] wrong answer"
        );
    }
}

/// #1250 and #1251 were both this assertion, failing in one configuration.
#[tokio::test]
async fn cancellation_reaches_a_running_handler_everywhere() {
    for (name, serve) in configurations() {
        let cancel =
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}}"#;
        let frames = exchange(
            serve,
            &[INIT.to_string(), INITIALIZED.to_string(), call(2, "wait")],
            &[cancel.to_string()],
            2,
        )
        .await;
        let answer = frames
            .iter()
            .find(|f| id_of(f) == Some(2))
            .unwrap_or_else(|| panic!("[{name}] cancellation never landed: {frames:?}"));
        assert_eq!(
            answer["result"]["content"][0]["text"], "cancelled",
            "[{name}] handler did not observe cancellation"
        );
    }
}

/// Cancellation must overtake queued work, which is what a concurrency cap
/// previously prevented.
#[tokio::test]
async fn cancellation_overtakes_a_queued_request_everywhere() {
    for (name, serve) in configurations() {
        let cancel =
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}}"#;
        let frames = exchange(
            serve,
            &[INIT.to_string(), INITIALIZED.to_string(), call(2, "wait")],
            &[call(3, "echo"), cancel.to_string()],
            3,
        )
        .await;
        for id in [2, 3] {
            assert!(
                frames.iter().any(|f| id_of(f) == Some(id)),
                "[{name}] no answer for {id}: {frames:?}"
            );
        }
    }
}

/// Responses are paired by id, never by arrival position.
#[tokio::test]
async fn every_configuration_answers_every_request_of_a_batch_of_calls() {
    for (name, serve) in configurations() {
        let frames = exchange(
            serve,
            &[
                INIT.to_string(),
                INITIALIZED.to_string(),
                call(2, "slow"),
                call(3, "echo"),
                call(4, "echo"),
            ],
            &[],
            4,
        )
        .await;
        for id in [2, 3, 4] {
            let answer = frames
                .iter()
                .find(|f| id_of(f) == Some(id))
                .unwrap_or_else(|| panic!("[{name}] missing {id}: {frames:?}"));
            assert!(
                answer["result"].is_object(),
                "[{name}] {id} did not succeed: {answer}"
            );
        }
    }
}

/// A request still running when input ends is answered rather than dropped,
/// in every configuration including the one with a drain deadline.
#[tokio::test]
async fn an_in_flight_request_survives_end_of_input_everywhere() {
    for (name, serve) in configurations() {
        let (mut writer, server_stdin) = tokio::io::duplex(8192);
        let (server_stdout, reader) = tokio::io::duplex(8192);
        tokio::spawn(serve(router(), server_stdin, server_stdout));

        for line in [INIT.to_string(), INITIALIZED.to_string(), call(2, "slow")] {
            writer.write_all(line.as_bytes()).await.unwrap();
            writer.write_all(b"\n").await.unwrap();
        }
        writer.flush().await.unwrap();
        drop(writer); // EOF while `slow` is still running

        let frames = timeout(
            Duration::from_secs(10),
            read_n_frames(BufReader::new(reader), 2),
        )
        .await
        .unwrap_or_else(|_| panic!("[{name}] timed out draining"));

        let answer = frames
            .iter()
            .find(|f| id_of(f) == Some(2))
            .unwrap_or_else(|| panic!("[{name}] in-flight request dropped: {frames:?}"));
        assert_eq!(answer["result"]["content"][0]["text"], "slow", "[{name}]");
    }
}
