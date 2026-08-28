//! Frame-level tracing must record metadata without ever recording a frame
//! body, across every async stdio variant.
//!
//! This is deliberately its own test binary rather than a module in
//! `stdio_loop.rs`. The assertions read back what a `tracing` subscriber
//! captured, and both the subscriber default and the callsite interest cache
//! are process-wide. Sharing a process with forty-odd other tests that drive
//! transports meant those callsites could be evaluated, and their interest
//! cached, on a thread with no subscriber installed, after which the DEBUG
//! frame events never reached the capture and the assertions saw zero of
//! them. It failed roughly one run in seven (#1435).
//!
//! Cargo gives every `tests/*.rs` its own binary, so the isolation is free:
//! nothing else in this process emits a tracing event.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;
use tower_mcp::transport::stdio::BidirectionalStdioTransport;
use tower_mcp::{McpRouter, StdioTransport};

/// Minimal router; these tests assert on what is traced, not on what is
/// served.
fn router() -> McpRouter {
    McpRouter::new().server_info("stdio-frame-tracing-test", "0.0.0")
}

/// Read newline-delimited JSON-RPC frames until `expected` are seen or the
/// stream ends.
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
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push(
            serde_json::from_str(trimmed)
                .unwrap_or_else(|e| panic!("invalid JSON on output: {e}: {trimmed}")),
        );
    }
    out
}

use std::io::Write;
use std::sync::{Arc, Mutex};
use tower_mcp::GenericStdioTransport;
use tower_mcp::context::{ServerNotification, notification_channel};
use tower_mcp::protocol::{LogLevel, LoggingMessageParams};

/// Read exactly one frame, failing rather than returning on EOF.
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

static TRACE_CAPTURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Default)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CaptureWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("trace capture lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CaptureWriter {
    fn contents(&self) -> String {
        String::from_utf8(self.0.lock().expect("trace capture lock").clone())
            .expect("tracing output is UTF-8")
    }
}

async fn trace_ping<F, Fut>(sentinel: &str, run: F) -> (usize, usize)
where
    F: FnOnce(tokio::io::DuplexStream, tokio::io::DuplexStream) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let frame = format!(r#"{{"jsonrpc":"2.0","id":"{sentinel}","method":"ping"}}"#);
    let (mut stdin_writer, server_stdin) = tokio::io::duplex(4096);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(4096);
    let transport = tokio::spawn(run(server_stdin, server_stdout));

    stdin_writer.write_all(frame.as_bytes()).await.unwrap();
    stdin_writer.write_all(b"\n").await.unwrap();
    stdin_writer.flush().await.unwrap();
    drop(stdin_writer);

    let frames = timeout(
        Duration::from_secs(5),
        read_n_frames(BufReader::new(server_stdout_reader), 1),
    )
    .await
    .expect("stdio transport must answer the ping");
    timeout(Duration::from_secs(5), transport)
        .await
        .expect("stdio transport must stop at EOF")
        .expect("transport task");

    assert_eq!(frames.len(), 1, "expected one ping response: {frames:?}");
    assert_eq!(frames[0]["id"], sentinel);
    let response_bytes = serde_json::to_string(&frames[0]).unwrap().len();
    (frame.len(), response_bytes)
}

/// The three async implementations each run their real read-eval-write
/// loop. A secret request ID appears in both wire frames, making one
/// sentinel cover inbound request and outbound response disclosure.
#[tokio::test(flavor = "current_thread")]
async fn async_stdio_variants_trace_metadata_without_frame_bodies() {
    // The non-ASCII sentinel makes the length assertion distinguish UTF-8
    // bytes from Unicode scalar values.
    const PLAIN_SECRET: &str = "plain-stdio-secret-é-e8c2";
    const LAYERED_SECRET: &str = "layered-stdio-secret-b50a";
    const GENERIC_SECRET: &str = "generic-stdio-secret-7f43";
    const BIDI_SECRET: &str = "bidi-stdio-secret-1d69";

    let _trace_capture = TRACE_CAPTURE_LOCK.lock().await;
    let captured = CaptureWriter::default();
    let trace_output = captured.clone();
    let tracing = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || captured.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(tracing);

    let mut byte_lengths = Vec::new();
    byte_lengths.push(
        trace_ping(PLAIN_SECRET, |stdin, stdout| async move {
            StdioTransport::new(router())
                .run_with_streams(stdin, stdout)
                .await
                .expect("plain stdio loop");
        })
        .await,
    );
    byte_lengths.push(
        trace_ping(LAYERED_SECRET, |stdin, stdout| async move {
            StdioTransport::new(router())
                .layer(tower::layer::util::Identity::new())
                .run_with_streams(stdin, stdout)
                .await
                .expect("layered stdio loop");
        })
        .await,
    );
    byte_lengths.push(
        trace_ping(GENERIC_SECRET, |stdin, stdout| async move {
            GenericStdioTransport::new(router())
                .run_with_streams(stdin, stdout)
                .await
                .expect("generic stdio loop");
        })
        .await,
    );
    byte_lengths.push(
        trace_ping(BIDI_SECRET, |stdin, stdout| async move {
            BidirectionalStdioTransport::new(router())
                .run_with_streams(stdin, stdout)
                .await
                .expect("bidirectional stdio loop");
        })
        .await,
    );

    let traces = trace_output.contents();
    for secret in [PLAIN_SECRET, LAYERED_SECRET, GENERIC_SECRET, BIDI_SECRET] {
        assert!(!traces.contains(secret), "frame body leaked: {traces}");
    }
    assert!(
        !traces.contains("input="),
        "raw input field returned: {traces}"
    );
    assert!(
        !traces.contains("output="),
        "raw output field returned: {traces}"
    );
    assert_eq!(
        traces.matches("direction=\"inbound\"").count(),
        4,
        "{traces}"
    );
    assert_eq!(
        traces.matches("direction=\"outbound\"").count(),
        4,
        "{traces}"
    );
    assert_eq!(
        traces.matches("frame_kind=\"message\"").count(),
        4,
        "{traces}"
    );
    assert_eq!(
        traces.matches("frame_kind=\"response\"").count(),
        4,
        "{traces}"
    );
    for (inbound, outbound) in byte_lengths {
        assert!(
            traces.contains(&format!("utf8_bytes={inbound}")),
            "{traces}"
        );
        assert!(
            traces.contains(&format!("utf8_bytes={outbound}")),
            "{traces}"
        );
    }
}

/// The bidirectional-only server-to-client request path has a separate
/// serializer and log site, so exercise it directly with both a sensitive
/// method and sensitive parameters.
#[tokio::test(flavor = "current_thread")]
async fn bidi_outgoing_request_trace_omits_method_and_params() {
    const SECRET_METHOD: &str = "private/provider-request-57d1";
    const SECRET_PARAM: &str = "provider-session-token-43d9";

    let _trace_capture = TRACE_CAPTURE_LOCK.lock().await;
    let captured = CaptureWriter::default();
    let trace_output = captured.clone();
    let tracing = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || captured.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(tracing);

    let mut transport = BidirectionalStdioTransport::new(router());
    let requester = transport.client_requester();
    let (mut stdin_writer, server_stdin) = tokio::io::duplex(4096);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(4096);
    let transport = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
            .expect("bidirectional stdio loop");
    });
    let request = tokio::spawn(async move {
        requester
            .request(
                SECRET_METHOD.to_string(),
                serde_json::json!({ "credential": SECRET_PARAM }),
            )
            .await
    });

    let mut stdout_reader = BufReader::new(server_stdout_reader);
    let outgoing = timeout(Duration::from_secs(5), read_frame(&mut stdout_reader))
        .await
        .expect("server-to-client request");
    assert_eq!(outgoing["method"], SECRET_METHOD);
    assert_eq!(outgoing["params"]["credential"], SECRET_PARAM);

    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": outgoing["id"],
        "result": { "accepted": true }
    });
    stdin_writer
        .write_all(format!("{response}\n").as_bytes())
        .await
        .unwrap();
    stdin_writer.flush().await.unwrap();
    timeout(Duration::from_secs(5), request)
        .await
        .expect("request round trip")
        .expect("request task")
        .expect("request result");
    drop(stdin_writer);
    timeout(Duration::from_secs(5), transport)
        .await
        .expect("transport stops at EOF")
        .expect("transport task");

    let traces = trace_output.contents();
    assert!(!traces.contains(SECRET_METHOD), "method leaked: {traces}");
    assert!(!traces.contains(SECRET_PARAM), "params leaked: {traces}");
    assert!(traces.contains("direction=\"outbound\""), "{traces}");
    assert!(traces.contains("frame_kind=\"request\""), "{traces}");
}

/// Notification bodies have historically carried terminal output and log
/// data, so cover that output category independently from responses.
#[tokio::test(flavor = "current_thread")]
async fn generic_notification_trace_omits_notification_body() {
    const SECRET: &str = "notification-terminal-output-82ac";

    let _trace_capture = TRACE_CAPTURE_LOCK.lock().await;
    let captured = CaptureWriter::default();
    let trace_output = captured.clone();
    let tracing = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || captured.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(tracing);

    let (notification_tx, notification_rx) = notification_channel(4);
    let mut transport = GenericStdioTransport::with_notifications(router(), notification_rx);
    let (stdin_writer, server_stdin) = tokio::io::duplex(4096);
    let (server_stdout, server_stdout_reader) = tokio::io::duplex(4096);
    let transport = tokio::spawn(async move {
        transport
            .run_with_streams(server_stdin, server_stdout)
            .await
            .expect("generic stdio loop");
    });

    notification_tx
        .send(ServerNotification::LogMessage(LoggingMessageParams {
            level: LogLevel::Info,
            logger: Some("test".to_string()),
            data: serde_json::json!(SECRET),
            meta: None,
        }))
        .await
        .expect("notification receiver");
    let mut stdout_reader = BufReader::new(server_stdout_reader);
    let notification = timeout(Duration::from_secs(5), read_frame(&mut stdout_reader))
        .await
        .expect("notification frame");
    assert_eq!(notification["params"]["data"], SECRET);

    drop(stdin_writer);
    timeout(Duration::from_secs(5), transport)
        .await
        .expect("transport stops at EOF")
        .expect("transport task");

    let traces = trace_output.contents();
    assert!(!traces.contains(SECRET), "notification leaked: {traces}");
    assert!(traces.contains("direction=\"outbound\""), "{traces}");
    assert!(traces.contains("frame_kind=\"notification\""), "{traces}");
}
