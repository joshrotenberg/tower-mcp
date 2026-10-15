//! What happens to in-flight handlers when `drain_timeout` expires (#1557).
//!
//! At EOF the stdio transports stop reading and drain the responses still
//! owed (#1252). When a configured `drain_timeout` runs out they stop waiting,
//! and the handlers they were waiting for have to be stopped too: their
//! cancellation token fires, and a handler that ignores it is aborted.
//!
//! Each scenario runs against all three transports. Every wait is bounded, so
//! a regression surfaces as a failed assertion rather than a stuck run.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;
use tower_mcp::extract::{Context as RequestContext, RawArgs};
use tower_mcp::{
    BidirectionalStdioTransport, CallToolResult, McpRouter, StdioTransport, ToolBuilder,
};

const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"drain-cancel","version":"0"}}}"#;
const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

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

#[derive(Clone, Copy, Debug)]
enum Kind {
    Plain,
    Generic,
    Bidirectional,
}

const KINDS: [Kind; 3] = [Kind::Plain, Kind::Generic, Kind::Bidirectional];

/// What a run produced and how long `run_with_streams` took to return.
struct Outcome {
    frames: Vec<serde_json::Value>,
    elapsed: Duration,
}

fn call(id: u64, name: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{{}}}}}}"#
    )
}

/// Pipe `INIT`, the initialized notification, and one call to `tool`, then
/// close stdin. EOF is the trigger for the drain under test.
async fn run_piped_call(kind: Kind, router: McpRouter, drain: Duration, tool: &str) -> Outcome {
    let input = format!("{INIT}\n{INITIALIZED}\n{}\n", call(2, tool));
    let output = CaptureWriter::default();
    let writer = output.clone();
    let reader = io::Cursor::new(input);

    let started = Instant::now();
    let run = async move {
        match kind {
            Kind::Plain => {
                StdioTransport::new(router)
                    .drain_timeout(drain)
                    .run_with_streams(reader, writer)
                    .await
            }
            Kind::Generic => {
                StdioTransport::new(router)
                    .drain_timeout(drain)
                    .layer(tower::layer::util::Identity::new())
                    .run_with_streams(reader, writer)
                    .await
            }
            Kind::Bidirectional => {
                BidirectionalStdioTransport::new(router)
                    .drain_timeout(drain)
                    .run_with_streams(reader, writer)
                    .await
            }
        }
    };
    timeout(Duration::from_secs(10), run)
        .await
        .unwrap_or_else(|_| panic!("{kind:?}: run did not return"))
        .unwrap_or_else(|e| panic!("{kind:?}: run failed: {e}"));
    Outcome {
        frames: output.frames(),
        elapsed: started.elapsed(),
    }
}

/// Poll `flag` until it is set or `budget` runs out.
async fn flag_set_within(flag: &AtomicBool, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    flag.load(Ordering::SeqCst)
}

/// Awaits its cancellation token and records that it fired.
fn cooperative_tool(started: Arc<AtomicBool>, saw_cancel: Arc<AtomicBool>) -> tower_mcp::Tool {
    ToolBuilder::new("cooperative")
        .description("Waits to be cancelled")
        .extractor_handler((), move |ctx: RequestContext, RawArgs(_): RawArgs| {
            let started = started.clone();
            let saw_cancel = saw_cancel.clone();
            async move {
                started.store(true, Ordering::SeqCst);
                ctx.cancellation_token().cancelled().await;
                saw_cancel.store(true, Ordering::SeqCst);
                Ok(CallToolResult::text("cancelled"))
            }
        })
        .build()
}

/// Sets a flag when dropped, which is how a test sees a task aborted.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Never looks at its cancellation token and never finishes.
fn stubborn_tool(dropped: Arc<AtomicBool>) -> tower_mcp::Tool {
    ToolBuilder::new("stubborn")
        .description("Ignores cancellation")
        .extractor_handler((), move |_ctx: RequestContext, RawArgs(_): RawArgs| {
            let dropped = dropped.clone();
            async move {
                let _guard = DropFlag(dropped);
                std::future::pending::<()>().await;
                Ok(CallToolResult::text("unreachable"))
            }
        })
        .build()
}

/// Finishes on its own shortly after starting.
fn quick_tool(saw_cancel: Arc<AtomicBool>) -> tower_mcp::Tool {
    ToolBuilder::new("quick")
        .description("Finishes within the drain")
        .extractor_handler((), move |ctx: RequestContext, RawArgs(_): RawArgs| {
            let saw_cancel = saw_cancel.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                if ctx.is_cancelled() {
                    saw_cancel.store(true, Ordering::SeqCst);
                }
                Ok(CallToolResult::text("done"))
            }
        })
        .build()
}

#[tokio::test]
async fn drain_timeout_fires_the_cancellation_token_of_what_is_still_running() {
    for kind in KINDS {
        let started = Arc::new(AtomicBool::new(false));
        let saw_cancel = Arc::new(AtomicBool::new(false));
        let router = McpRouter::new()
            .server_info("drain-cancel", "0")
            .tool(cooperative_tool(started.clone(), saw_cancel.clone()));

        let outcome = run_piped_call(kind, router, Duration::from_millis(200), "cooperative").await;

        assert!(
            started.load(Ordering::SeqCst),
            "{kind:?}: the handler never started"
        );
        assert!(
            saw_cancel.load(Ordering::SeqCst),
            "{kind:?}: the handler outlived `run` without its token firing"
        );
        // The wait ends at the drain timeout, not at the end of the grace
        // period a handler that honors cancellation does not need.
        assert!(
            outcome.elapsed < Duration::from_secs(2),
            "{kind:?}: run took {:?}",
            outcome.elapsed
        );
    }
}

#[tokio::test]
async fn drain_timeout_aborts_a_handler_that_ignores_cancellation() {
    for kind in KINDS {
        let dropped = Arc::new(AtomicBool::new(false));
        let router = McpRouter::new()
            .server_info("drain-cancel", "0")
            .tool(stubborn_tool(dropped.clone()));

        let outcome = run_piped_call(kind, router, Duration::from_millis(200), "stubborn").await;

        assert!(
            outcome.elapsed >= Duration::from_millis(150),
            "{kind:?}: run returned before the drain timeout: {:?}",
            outcome.elapsed
        );
        // Bounded by the drain, the cancellation grace period, and slack.
        assert!(
            outcome.elapsed < Duration::from_secs(5),
            "{kind:?}: run took {:?}",
            outcome.elapsed
        );
        assert!(
            flag_set_within(&dropped, Duration::from_secs(2)).await,
            "{kind:?}: the handler was left running after `run` returned"
        );
        assert!(
            outcome.frames.iter().all(|f| f["id"] != 2),
            "{kind:?}: an abandoned request was answered: {:?}",
            outcome.frames
        );
    }
}

/// The bound only applies once it elapses: a handler that finishes inside it
/// is answered and never sees its token fire.
#[tokio::test]
async fn a_handler_that_finishes_within_the_drain_is_answered_not_cancelled() {
    for kind in KINDS {
        let saw_cancel = Arc::new(AtomicBool::new(false));
        let router = McpRouter::new()
            .server_info("drain-cancel", "0")
            .tool(quick_tool(saw_cancel.clone()));

        let outcome = run_piped_call(kind, router, Duration::from_secs(5), "quick").await;

        let response = outcome
            .frames
            .iter()
            .find(|f| f["id"] == 2)
            .unwrap_or_else(|| panic!("{kind:?}: no response: {:?}", outcome.frames));
        assert_eq!(
            response["result"]["content"][0]["text"], "done",
            "{kind:?}: {response}"
        );
        assert!(
            !saw_cancel.load(Ordering::SeqCst),
            "{kind:?}: the token fired before the drain timed out"
        );
        assert!(
            outcome.elapsed < Duration::from_secs(3),
            "{kind:?}: run took {:?}",
            outcome.elapsed
        );
    }
}

/// Two connections built from clones of one router share its in-flight
/// registry. One connection giving up on its handlers must not cancel the
/// other's.
#[tokio::test]
async fn giving_up_on_one_connection_does_not_cancel_another_sharing_the_router() {
    let started = Arc::new(AtomicBool::new(false));
    let saw_cancel = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let router = McpRouter::new()
        .server_info("drain-cancel", "0")
        .tool(cooperative_tool(started.clone(), saw_cancel.clone()))
        .tool(stubborn_tool(dropped.clone()));

    // Connection B keeps its stdin open, so its handler stays in flight.
    let (mut b_input, b_reader) = tokio::io::duplex(8192);
    let b_output = CaptureWriter::default();
    let b_writer = b_output.clone();
    let mut b_transport = StdioTransport::new(router.clone());
    let b = tokio::spawn(async move { b_transport.run_with_streams(b_reader, b_writer).await });
    b_input
        .write_all(format!("{INIT}\n{INITIALIZED}\n{}\n", call(2, "cooperative")).as_bytes())
        .await
        .unwrap();
    assert!(
        flag_set_within(&started, Duration::from_secs(5)).await,
        "connection B's handler never started"
    );

    // Connection A drains and gives up on its own handler.
    let outcome = run_piped_call(
        Kind::Plain,
        router.clone(),
        Duration::from_millis(100),
        "stubborn",
    )
    .await;
    assert!(
        flag_set_within(&dropped, Duration::from_secs(2)).await,
        "connection A's handler was left running"
    );
    assert!(outcome.elapsed < Duration::from_secs(5));

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !saw_cancel.load(Ordering::SeqCst),
        "connection B's handler was cancelled by connection A"
    );
    assert!(!b.is_finished(), "connection B ended");

    // Closing B's stdin without a timeout waits for its handler, so the
    // handler is released by cancelling it the way a client would.
    b_input
        .write_all(
            br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}}"#,
        )
        .await
        .unwrap();
    b_input.write_all(b"\n").await.unwrap();
    assert!(
        flag_set_within(&saw_cancel, Duration::from_secs(5)).await,
        "a request-scoped cancellation no longer reaches the handler"
    );
    drop(b_input);
    timeout(Duration::from_secs(5), b)
        .await
        .expect("connection B did not finish")
        .unwrap()
        .unwrap();
}
