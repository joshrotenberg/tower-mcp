//! Env-gated capture of every JSON-RPC frame the HTTP transport reads or writes.
//!
//! When `TOWER_MCP_FRAME_LOG` names a file, an axum middleware around the MCP
//! router appends one JSON line per frame:
//!
//! ```json
//! {"dir":"in","version":"2025-11-25","session":"abc","exchange":7,"frame":{...}}
//! ```
//!
//! - `dir`: `in` for a frame the server received, `out` for one it sent.
//! - `version`: the protocol version in effect for the frame. It comes from the
//!   `MCP-Protocol-Version` request header, falling back to the version the
//!   session negotiated in its `initialize` result. `null` when neither is
//!   known (the `initialize` request itself); `wire-schema-check` fills those
//!   in from the rest of the exchange.
//! - `session`: the `Mcp-Session-Id` header (request, or response for the
//!   `initialize` reply), `null` on the sessionless 2026-07-28 path.
//! - `exchange`: a counter shared by every frame of one HTTP request/response
//!   pair. A response correlates to a request by `(exchange, id)`, or by
//!   `(session, id)` when the reply travels on a later HTTP exchange.
//! - `frame`: the JSON-RPC message exactly as parsed from the wire bytes.
//!
//! The capture sits on the raw HTTP bodies, so what is logged is what was
//! serialized, not a re-serialization of typed values. Response bodies are
//! teed rather than buffered: SSE streams keep streaming and each `data:`
//! event is logged as soon as it is complete.
//!
//! Nothing is installed when the variable is unset.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::http::header::CONTENT_TYPE;
use axum::middleware::Next;
use axum::response::Response;
use futures::StreamExt;
use serde_json::{Value, json};

/// Name of the environment variable that enables capture.
pub const ENV_VAR: &str = "TOWER_MCP_FRAME_LOG";

const PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
const SESSION_ID_HEADER: &str = "mcp-session-id";
/// Upper bound on a buffered request or JSON response body.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Shared capture state.
#[derive(Clone)]
pub struct FrameLog {
    inner: Arc<Inner>,
}

struct Inner {
    file: Mutex<File>,
    next_exchange: AtomicU64,
    /// Negotiated version per session, learned from `initialize` results.
    session_versions: Mutex<HashMap<String, String>>,
}

impl FrameLog {
    /// Open the log named by `TOWER_MCP_FRAME_LOG`, or `None` when unset.
    pub fn from_env() -> std::io::Result<Option<Self>> {
        match std::env::var_os(ENV_VAR).filter(|p| !p.is_empty()) {
            Some(path) => Self::open(path.as_ref()).map(Some),
            None => Ok(None),
        }
    }

    /// Open (creating if needed) `path` for appending.
    fn open(path: &std::path::Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            inner: Arc::new(Inner {
                file: Mutex::new(file),
                next_exchange: AtomicU64::new(1),
                session_versions: Mutex::new(HashMap::new()),
            }),
        })
    }

    fn write(&self, dir: &str, ctx: &Context, version: Option<&str>, frame: &Value) {
        let line = json!({
            "dir": dir,
            "version": version,
            "session": ctx.session,
            "exchange": ctx.exchange,
            "frame": frame,
        });
        let mut text = line.to_string();
        text.push('\n');
        // One `write_all` per line keeps lines whole under the mutex.
        if let Ok(mut file) = self.inner.file.lock() {
            let _ = file.write_all(text.as_bytes());
        }
    }

    /// Log each JSON-RPC message found in a decoded body (an object, or a
    /// legacy batch array).
    fn log_value(&self, dir: &str, ctx: &Context, header_version: Option<&str>, value: Value) {
        let frames = match value {
            Value::Array(items) => items,
            other => vec![other],
        };
        for frame in frames {
            if !frame.is_object() {
                continue;
            }
            let session_version = ctx.session.as_deref().and_then(|s| self.session_version(s));
            // An initialize result carries the negotiated version; remember
            // it for later frames of the session that omit the header.
            let negotiated = frame
                .pointer("/result/protocolVersion")
                .and_then(Value::as_str);
            if let (Some(session), Some(version)) = (ctx.session.as_deref(), negotiated)
                && let Ok(mut map) = self.inner.session_versions.lock()
            {
                map.insert(session.to_string(), version.to_string());
            }
            let version = negotiated
                .map(str::to_string)
                .or_else(|| header_version.map(str::to_string))
                .or(session_version);
            self.write(dir, ctx, version.as_deref(), &frame);
        }
    }

    fn session_version(&self, session: &str) -> Option<String> {
        self.inner
            .session_versions
            .lock()
            .ok()?
            .get(session)
            .cloned()
    }
}

#[derive(Clone)]
struct Context {
    session: Option<String>,
    exchange: u64,
}

fn header_string(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Axum middleware: record the request body, run the handler, tee the response.
pub async fn capture(State(log): State<FrameLog>, request: Request, next: Next) -> Response {
    let exchange = log.inner.next_exchange.fetch_add(1, Ordering::Relaxed);
    let request_version = header_string(request.headers(), PROTOCOL_VERSION_HEADER);
    let request_session = header_string(request.headers(), SESSION_ID_HEADER);
    let mut ctx = Context {
        session: request_session,
        exchange,
    };

    let (parts, body) = request.into_parts();
    let bytes = match to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        // Oversized or broken body: forward an empty one and let the
        // transport produce its own error.
        Err(_) => Bytes::new(),
    };
    if !bytes.is_empty()
        && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
    {
        log.log_value("in", &ctx, request_version.as_deref(), value);
    }
    let request = Request::from_parts(parts, Body::from(bytes));

    let response = next.run(request).await;

    // The initialize reply is where the server assigns the session id.
    if ctx.session.is_none() {
        ctx.session = header_string(response.headers(), SESSION_ID_HEADER);
    }

    let is_sse = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if !is_sse && !is_json {
        return response;
    }

    let (parts, body) = response.into_parts();
    let mut tee = Tee {
        log,
        ctx,
        header_version: request_version,
        sse: is_sse,
        pending: Vec::new(),
        event_data: String::new(),
    };
    let stream = body.into_data_stream().map(move |chunk| {
        if let Ok(bytes) = &chunk {
            tee.feed(bytes);
        }
        chunk
    });
    // The closure owns the tee; dropping the stream (end of body or client
    // disconnect) flushes whatever is left. The bytes the client receives are
    // untouched.
    Response::from_parts(parts, Body::from_stream(stream))
}

/// Incremental frame extractor for one response body.
struct Tee {
    log: FrameLog,
    ctx: Context,
    header_version: Option<String>,
    sse: bool,
    /// Bytes not yet consumed: an incomplete SSE line, or the whole JSON body.
    pending: Vec<u8>,
    /// `data:` lines of the SSE event being assembled.
    event_data: String,
}

impl Tee {
    fn feed(&mut self, chunk: &[u8]) {
        if self.sse {
            self.feed_sse(chunk);
        } else if self.pending.len() + chunk.len() <= MAX_BODY_BYTES {
            self.pending.extend_from_slice(chunk);
        }
    }

    fn feed_sse(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
        while let Some(pos) = self.pending.iter().position(|b| *b == b'\n') {
            let line_bytes: Vec<u8> = self.pending.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                self.flush_event();
            } else if let Some(data) = line.strip_prefix("data:") {
                if !self.event_data.is_empty() {
                    self.event_data.push('\n');
                }
                self.event_data
                    .push_str(data.strip_prefix(' ').unwrap_or(data));
            }
        }
    }

    fn flush_event(&mut self) {
        let data = std::mem::take(&mut self.event_data);
        if data.trim().is_empty() {
            return;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&data) {
            self.log
                .log_value("out", &self.ctx, self.header_version.as_deref(), value);
        }
    }

    /// Called when the body stream ends.
    fn finish(&mut self) {
        if self.sse {
            // A final line or event that was never terminated.
            if !self.pending.is_empty() {
                self.feed_sse(b"\n");
            }
            self.flush_event();
        } else if !self.pending.is_empty()
            && let Ok(value) = serde_json::from_slice::<Value>(&self.pending)
        {
            self.log
                .log_value("out", &self.ctx, self.header_version.as_deref(), value);
            self.pending.clear();
        }
    }
}

impl Drop for Tee {
    fn drop(&mut self) {
        // Runs when the body stream completes or the client disconnects.
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log(name: &str) -> (FrameLog, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("frame-log-{}-{name}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        (FrameLog::open(&path).unwrap(), path)
    }

    fn lines(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn tee(log: &FrameLog, sse: bool) -> Tee {
        Tee {
            log: log.clone(),
            ctx: Context {
                session: Some("s".into()),
                exchange: 3,
            },
            header_version: Some("2025-11-25".into()),
            sse,
            pending: Vec::new(),
            event_data: String::new(),
        }
    }

    #[test]
    fn sse_events_split_across_chunks_are_logged_whole() {
        let (log, path) = temp_log("sse");
        let mut t = tee(&log, true);
        t.feed(b"id: 1\ndata: \n\nevent: message\ndata: {\"jsonrpc\":");
        t.feed(b"\"2.0\",\"method\":\"notifications/progress\"}\r\n\r\n");
        t.feed(b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}");
        drop(t); // the unterminated last event is flushed on drop
        let got = lines(&path);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["dir"], "out");
        assert_eq!(got[0]["version"], "2025-11-25");
        assert_eq!(got[0]["session"], "s");
        assert_eq!(got[0]["exchange"], 3);
        assert_eq!(got[0]["frame"]["method"], "notifications/progress");
        assert_eq!(got[1]["frame"]["id"], 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn json_body_is_logged_when_the_stream_ends() {
        let (log, path) = temp_log("json");
        let mut t = tee(&log, false);
        t.feed(b"{\"jsonrpc\":\"2.0\",");
        assert!(lines(&path).is_empty());
        t.feed(b"\"id\":2,\"result\":{}}");
        drop(t);
        let got = lines(&path);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["frame"]["id"], 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn initialize_result_sets_the_session_version() {
        let (log, path) = temp_log("version");
        let ctx = Context {
            session: Some("s".into()),
            exchange: 1,
        };
        log.log_value(
            "out",
            &ctx,
            None,
            json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": "2025-03-26"}}),
        );
        // A later frame with no header takes the negotiated version.
        log.log_value(
            "in",
            &ctx,
            None,
            json!({"jsonrpc": "2.0", "method": "ping", "id": 2}),
        );
        let got = lines(&path);
        assert_eq!(got[0]["version"], "2025-03-26");
        assert_eq!(got[1]["version"], "2025-03-26");
        let _ = std::fs::remove_file(path);
    }
}
