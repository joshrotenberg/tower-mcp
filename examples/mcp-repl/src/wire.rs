//! Wire tracing: the raw JSON-RPC frames, redacted, plus the last exchange.
//!
//! Half of any "is it the client, the server, or the network?" question is
//! answered by seeing the frames themselves. [`TracingTransport`] wraps any
//! [`ClientTransport`] and reports every frame that crosses it to a [`Wire`],
//! which records the last request/response pair and, when tracing is on,
//! renders the frame for printing.
//!
//! Recording happens whether or not tracing is on, so `last` can reprint an
//! exchange the user did not know they would want. The cost is one JSON parse
//! per frame, which is nothing next to the round trip that produced it.
//!
//! Rendered frames are meant for stderr: `--json` output goes to stdout, and
//! a trace interleaved with it would break whatever is parsing it downstream.
//!
//! Secrets are masked before a frame is stored, so a redacted frame is the
//! only form that exists past this module.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nu_ansi_term::Style;
use serde_json::Value;
use tower_mcp::client::ClientTransport;
use tower_mcp::error::Result;

use crate::style::{paint, tag};
use crate::timing;

/// Which way a frame went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Client to server.
    Sent,
    /// Server to client.
    Received,
}

impl Direction {
    fn label(self) -> &'static str {
        match self {
            Direction::Sent => "wire ->",
            Direction::Received => "wire <-",
        }
    }
}

/// One frame: the parsed JSON with secrets already masked, how far into the
/// session it crossed the wire, and, for a response, how long its request had
/// been outstanding.
#[derive(Clone, Debug)]
pub struct Frame {
    pub json: Value,
    pub at: Duration,
    pub elapsed: Option<Duration>,
}

/// The frame recorder. One per process in normal use (see [`wire`]); tests
/// build their own so they do not share state.
pub struct Wire {
    trace: AtomicBool,
    started: Instant,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Outstanding request ids and when they were sent, for the elapsed
    /// annotation on the matching response.
    pending: HashMap<String, Instant>,
    last_request: Option<Frame>,
    last_response: Option<Frame>,
}

/// A server that never answers would otherwise grow `pending` without bound.
/// Well past any realistic number of concurrent requests from a REPL.
const PENDING_CAP: usize = 256;

impl Wire {
    pub fn new(trace: bool) -> Self {
        Self {
            trace: AtomicBool::new(trace),
            started: Instant::now(),
            state: Mutex::new(State::default()),
        }
    }

    pub fn set_trace(&self, on: bool) {
        self.trace.store(on, Ordering::Relaxed);
    }

    pub fn trace_enabled(&self) -> bool {
        self.trace.load(Ordering::Relaxed)
    }

    /// Record an outgoing frame. Returns the rendered trace block when
    /// tracing is on.
    pub fn sent(&self, raw: &str) -> Option<String> {
        let frame = self.record(Direction::Sent, raw);
        self.trace_enabled()
            .then(|| render(Direction::Sent, &frame))
    }

    /// Record an incoming frame. Returns the rendered trace block when
    /// tracing is on.
    pub fn received(&self, raw: &str) -> Option<String> {
        let frame = self.record(Direction::Received, raw);
        self.trace_enabled()
            .then(|| render(Direction::Received, &frame))
    }

    /// The most recent request and, if it has arrived, its response.
    pub fn last_exchange(&self) -> Option<(Frame, Option<Frame>)> {
        let state = self.state.lock().unwrap();
        let request = state.last_request.clone()?;
        Some((request, state.last_response.clone()))
    }

    fn record(&self, dir: Direction, raw: &str) -> Frame {
        let now = Instant::now();
        let json = redact(&parse(raw));
        let id = frame_id(&json);
        // A frame carrying a `method` is a request or a notification, whichever
        // side sent it. That is what separates our request from our response to
        // a server-initiated one, and a server's response from its own request.
        let has_method = json.get("method").is_some();

        let mut state = self.state.lock().unwrap();
        let mut elapsed = None;
        if dir == Direction::Received
            && !has_method
            && let Some(id) = &id
        {
            elapsed = state
                .pending
                .remove(id)
                .map(|sent| now.saturating_duration_since(sent));
        }
        let frame = Frame {
            json,
            at: now.saturating_duration_since(self.started),
            elapsed,
        };
        match dir {
            Direction::Sent => {
                if has_method && let Some(id) = id {
                    if state.pending.len() >= PENDING_CAP {
                        state.pending.clear();
                    }
                    state.pending.insert(id, now);
                    // A new request is a new exchange: the previous one stops
                    // being "last" the moment this goes out.
                    state.last_request = Some(frame.clone());
                    state.last_response = None;
                }
            }
            Direction::Received => {
                if !has_method
                    && id.is_some()
                    && state.last_request.as_ref().and_then(|f| frame_id(&f.json)) == id
                {
                    state.last_response = Some(frame.clone());
                }
            }
        }
        frame
    }
}

/// The process-wide recorder. Created by [`init`] at startup; the lazy
/// fallback keeps the accessor total for any path that runs before it.
static WIRE: OnceLock<Wire> = OnceLock::new();

pub fn init(trace: bool) {
    let _ = WIRE.set(Wire::new(trace));
}

pub fn wire() -> &'static Wire {
    WIRE.get_or_init(|| Wire::new(false))
}

/// A frame as it prints: a dim header with direction, session-relative
/// timestamp, and (for a response) the round-trip time, then the pretty JSON.
pub fn render(dir: Direction, frame: &Frame) -> String {
    let mut header = format!(
        "{} {}",
        tag(Style::new().dimmed(), dir.label()),
        paint(
            Style::new().dimmed(),
            &format!("+{:.3}s", frame.at.as_secs_f64())
        )
    );
    if let Some(elapsed) = frame.elapsed {
        header.push(' ');
        header.push_str(&timing(elapsed));
    }
    let body = serde_json::to_string_pretty(&frame.json).unwrap_or_else(|_| frame.json.to_string());
    format!("{header}\n{}", paint(Style::new().dimmed(), &body))
}

/// A frame that is not valid JSON still deserves to be seen: it is exactly
/// the case where the trace is the answer.
fn parse(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// The JSON-RPC id as a lookup key. Numbers and strings both appear in the
/// wild, and `null` means there is no correlation to make.
fn frame_id(json: &Value) -> Option<String> {
    match json.get("id")? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

const REDACTED: &str = "<redacted>";

/// Keys whose values never print, in normalized form (see [`normalize_key`]).
/// Matching is exact after normalization, so a `taskToken` argument is not
/// caught by `token`.
const SECRET_KEYS: &[&str] = &[
    "authorization",
    "proxyauthorization",
    "bearer",
    "bearertoken",
    "token",
    "accesstoken",
    "refreshtoken",
    "idtoken",
    "sessiontoken",
    "apikey",
    "xapikey",
    "secret",
    "clientsecret",
    "password",
    "passwd",
    "passphrase",
    "credential",
    "credentials",
];

/// Lowercase and drop separators, so `X-Api-Key`, `x_api_key`, and `apiKey`
/// all compare equal to the same entry.
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn is_secret_key(key: &str) -> bool {
    let normalized = normalize_key(key);
    SECRET_KEYS.contains(&normalized.as_str())
}

/// Mask secrets before a frame goes anywhere. Recursive: a token nested in a
/// tool's arguments is as sensitive as one in a header map.
fn redact(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, val)| {
                    if is_secret_key(key) {
                        (key.clone(), Value::String(REDACTED.to_string()))
                    } else {
                        (key.clone(), redact(val))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        Value::String(s) => Value::String(mask_bearer(s)),
        other => other.clone(),
    }
}

/// A header line echoed inside a string value (`"Authorization: Bearer abc"`
/// in an error message, say) carries a live token where the key-name rule
/// cannot see it. Everything after the scheme goes.
fn mask_bearer(s: &str) -> String {
    // ASCII-only lowercasing leaves byte offsets aligned with the original.
    let lowered = s.to_ascii_lowercase();
    match lowered.find("bearer ") {
        Some(at) => format!("{}{REDACTED}", &s[..at + "bearer ".len()]),
        None => s.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Transport wrapper
// ---------------------------------------------------------------------------

/// Wraps any client transport and reports each frame to a [`Wire`].
///
/// Every method delegates, including `supports_session_recovery`: the wrapper
/// must be invisible to the client's own session handling.
pub struct TracingTransport<T> {
    inner: T,
    wire: &'static Wire,
}

impl<T: ClientTransport> TracingTransport<T> {
    pub fn new(inner: T) -> Self {
        Self::with_wire(inner, wire())
    }

    pub fn with_wire(inner: T, wire: &'static Wire) -> Self {
        Self { inner, wire }
    }
}

#[async_trait]
impl<T: ClientTransport> ClientTransport for TracingTransport<T> {
    async fn send(&mut self, message: &str) -> Result<()> {
        if let Some(block) = self.wire.sent(message) {
            eprintln!("{block}");
        }
        self.inner.send(message).await
    }

    async fn recv(&mut self) -> Result<Option<String>> {
        let message = self.inner.recv().await?;
        if let Some(raw) = &message
            && let Some(block) = self.wire.received(raw)
        {
            eprintln!("{block}");
        }
        Ok(message)
    }

    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }

    async fn close(&mut self) -> Result<()> {
        self.inner.close().await
    }

    async fn reset_session(&mut self) {
        self.inner.reset_session().await;
    }

    fn supports_session_recovery(&self) -> bool {
        self.inner.supports_session_recovery()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: u32, method: &str) -> String {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": {}}).to_string()
    }

    fn response(id: u32) -> String {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}}).to_string()
    }

    #[test]
    fn a_response_is_paired_with_the_request_it_answers() {
        let wire = Wire::new(true);
        wire.sent(&request(1, "tools/call"));
        wire.received(&response(1));

        let (req, resp) = wire.last_exchange().expect("an exchange was recorded");
        assert_eq!(req.json["method"], "tools/call");
        let resp = resp.expect("the response was paired");
        assert_eq!(resp.json["result"]["ok"], true);
        assert!(
            resp.elapsed.is_some(),
            "a paired response carries its round-trip time"
        );
    }

    #[test]
    fn a_new_request_clears_the_previous_response() {
        let wire = Wire::new(false);
        wire.sent(&request(1, "tools/list"));
        wire.received(&response(1));
        wire.sent(&request(2, "tools/call"));

        let (req, resp) = wire.last_exchange().unwrap();
        assert_eq!(req.json["method"], "tools/call");
        assert!(resp.is_none(), "the new request has not been answered yet");
    }

    #[test]
    fn notifications_are_not_exchanges() {
        let wire = Wire::new(false);
        wire.sent(
            &serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
                .to_string(),
        );
        assert!(wire.last_exchange().is_none());
    }

    #[test]
    fn a_server_initiated_request_does_not_answer_ours() {
        let wire = Wire::new(false);
        wire.sent(&request(1, "tools/call"));
        // The server asks us something mid-call (sampling, elicitation). It
        // carries an id, but it is not our response.
        wire.received(&request(7, "sampling/createMessage"));

        let (_, resp) = wire.last_exchange().unwrap();
        assert!(resp.is_none());
    }

    #[test]
    fn a_mismatched_response_is_not_the_last_response() {
        let wire = Wire::new(false);
        wire.sent(&request(1, "tools/list"));
        wire.sent(&request(2, "tools/call"));
        // Answers the older request, which is no longer the tracked exchange.
        wire.received(&response(1));

        let (req, resp) = wire.last_exchange().unwrap();
        assert_eq!(req.json["id"], 2);
        assert!(resp.is_none());
    }

    #[test]
    fn recording_happens_with_tracing_off_but_nothing_renders() {
        let wire = Wire::new(false);
        assert!(wire.sent(&request(1, "tools/list")).is_none());
        assert!(wire.received(&response(1)).is_none());
        assert!(
            wire.last_exchange().is_some(),
            "`last` works without --trace"
        );

        wire.set_trace(true);
        assert!(wire.sent(&request(2, "tools/list")).is_some());
    }

    #[test]
    fn a_rendered_frame_shows_direction_timestamp_and_elapsed() {
        let wire = Wire::new(true);
        let sent = wire.sent(&request(1, "tools/call")).unwrap();
        assert!(sent.contains("wire ->"), "{sent}");
        assert!(sent.contains("+0."), "a session-relative timestamp: {sent}");
        assert!(sent.contains("tools/call"), "{sent}");
        assert!(!sent.contains("elapsed"));

        let received = wire.received(&response(1)).unwrap();
        assert!(received.contains("wire <-"), "{received}");
        assert!(
            received.contains("ms]") || received.contains("s]"),
            "a response carries its round-trip time: {received}"
        );
    }

    #[test]
    fn an_unparseable_frame_still_traces() {
        let wire = Wire::new(true);
        let rendered = wire.received("<html>502 Bad Gateway</html>").unwrap();
        assert!(rendered.contains("502 Bad Gateway"), "{rendered}");
    }

    #[test]
    fn secrets_are_masked_by_key_name() {
        let frame = redact(&serde_json::json!({
            "params": {
                "headers": {"Authorization": "Bearer sk-live-123", "X-Api-Key": "k1"},
                "arguments": {"apiKey": "k2", "password": "hunter2", "nested": [{"token": "t"}]},
            }
        }));
        let rendered = frame.to_string();
        for secret in ["sk-live-123", "k1", "k2", "hunter2", "\"t\""] {
            assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
        }
        assert_eq!(frame["params"]["headers"]["Authorization"], REDACTED);
        assert_eq!(frame["params"]["arguments"]["nested"][0]["token"], REDACTED);
    }

    #[test]
    fn a_bearer_token_inside_a_string_is_masked() {
        let frame = redact(&serde_json::json!({
            "error": {"message": "rejected Authorization: Bearer sk-live-123"}
        }));
        let message = frame["error"]["message"].as_str().unwrap();
        assert!(!message.contains("sk-live-123"), "{message}");
        assert!(message.starts_with("rejected Authorization: Bearer "));
    }

    #[test]
    fn ordinary_values_are_left_alone() {
        let original = serde_json::json!({
            "params": {"name": "add", "arguments": {"a": 2, "b": 3, "taskToken": "visible"}},
            "flags": [true, null, 1.5],
        });
        assert_eq!(redact(&original), original);
    }

    // -- the transport wrapper ------------------------------------------------

    struct FakeTransport {
        sent: Vec<String>,
        incoming: Vec<String>,
    }

    #[async_trait]
    impl ClientTransport for FakeTransport {
        async fn send(&mut self, message: &str) -> Result<()> {
            self.sent.push(message.to_string());
            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<String>> {
            Ok(if self.incoming.is_empty() {
                None
            } else {
                Some(self.incoming.remove(0))
            })
        }

        fn is_connected(&self) -> bool {
            true
        }

        async fn close(&mut self) -> Result<()> {
            Ok(())
        }

        fn supports_session_recovery(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn the_wrapper_records_both_directions_and_delegates() {
        // Leaked rather than global, so this test cannot collide with another.
        let wire: &'static Wire = Box::leak(Box::new(Wire::new(false)));
        let mut transport = TracingTransport::with_wire(
            FakeTransport {
                sent: Vec::new(),
                incoming: vec![response(1)],
            },
            wire,
        );

        transport.send(&request(1, "tools/call")).await.unwrap();
        let received = transport.recv().await.unwrap();

        assert_eq!(received.as_deref(), Some(response(1).as_str()));
        assert_eq!(
            transport.inner.sent.len(),
            1,
            "the frame reached the inner transport"
        );
        assert!(
            transport.supports_session_recovery(),
            "the wrapper must not change how the client handles sessions"
        );
        let (req, resp) = wire.last_exchange().unwrap();
        assert_eq!(req.json["method"], "tools/call");
        assert!(resp.is_some());
    }
}
