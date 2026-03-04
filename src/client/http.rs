//! HTTP client transport for remote MCP servers.
//!
//! Provides [`HttpClientTransport`] which connects to an MCP server using
//! the Streamable HTTP transport protocol (MCP spec 2025-11-25). Manages
//! session lifecycle, SSE stream for server notifications, and HTTP POST
//! for client requests.
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::client::{McpClient, HttpClientTransport};
//!
//! # async fn example() -> Result<(), tower_mcp::BoxError> {
//! let transport = HttpClientTransport::new("http://localhost:3000");
//! let client = McpClient::connect(transport).await?;
//!
//! let info = client.initialize("my-client", "1.0.0").await?;
//! println!("Connected to: {}", info.server_info.name);
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::transport::ClientTransport;
use crate::error::{Error, Result};

/// Configuration for [`HttpClientTransport`].
///
/// # Example
///
/// ```rust,no_run
/// use tower_mcp::client::{HttpClientTransport, HttpClientConfig};
/// use std::time::Duration;
///
/// let config = HttpClientConfig {
///     request_timeout: Duration::from_secs(60),
///     ..Default::default()
/// };
/// let transport = HttpClientTransport::with_config("http://localhost:3000", config);
/// ```
#[derive(Debug, Clone)]
pub struct HttpClientConfig {
    /// Custom headers to include on every request (e.g., auth tokens).
    pub headers: HashMap<String, String>,
    /// Whether to automatically open the SSE stream after initialization.
    /// Default: `true`.
    pub auto_sse: bool,
    /// Capacity of the internal message channel.
    /// Default: 256.
    pub channel_capacity: usize,
    /// Timeout for HTTP requests.
    /// Default: 30 seconds.
    pub request_timeout: Duration,
    /// Whether to attempt SSE reconnection on disconnect.
    /// Default: `true`.
    pub sse_reconnect: bool,
    /// Delay before SSE reconnection attempts.
    /// Default: 1 second.
    pub sse_reconnect_delay: Duration,
    /// Maximum SSE reconnection attempts before giving up.
    /// Default: 5.
    pub max_sse_reconnect_attempts: u32,
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            headers: HashMap::new(),
            auto_sse: true,
            channel_capacity: 256,
            request_timeout: Duration::from_secs(30),
            sse_reconnect: true,
            sse_reconnect_delay: Duration::from_secs(1),
            max_sse_reconnect_attempts: 5,
        }
    }
}

/// Client transport for MCP servers over Streamable HTTP.
///
/// Connects to a remote MCP server using the Streamable HTTP transport
/// protocol. Manages session lifecycle (`mcp-session-id`), opens an SSE
/// stream for server-initiated messages, and sends client requests via
/// HTTP POST.
///
/// # How it works
///
/// The transport bridges HTTP's request/response model with the
/// `ClientTransport` trait's `send()`/`recv()` message-passing model:
///
/// - **`send()`** POSTs JSON-RPC messages to the server and queues the
///   response body into an internal channel for `recv()` to return.
/// - **`recv()`** reads from that channel, which also receives SSE events
///   from a background task.
///
/// After the `initialize` handshake establishes a session, an SSE stream
/// is automatically opened to receive server notifications and
/// server-initiated requests.
///
/// # Example
///
/// ```rust,no_run
/// use tower_mcp::client::{McpClient, HttpClientTransport};
///
/// # async fn example() -> Result<(), tower_mcp::BoxError> {
/// let transport = HttpClientTransport::new("http://localhost:3000");
/// let client = McpClient::connect(transport).await?;
///
/// let info = client.initialize("my-client", "1.0.0").await?;
/// let tools = client.list_tools().await?;
/// client.shutdown().await?;
/// # Ok(())
/// # }
/// ```
pub struct HttpClientTransport {
    /// The base URL of the MCP server endpoint.
    url: String,
    /// reqwest HTTP client (reused across requests).
    client: reqwest::Client,
    /// Session ID received from the server after `initialize`.
    session_id: Option<String>,
    /// Negotiated protocol version.
    protocol_version: Option<String>,
    /// Channel receiver for incoming messages (POST responses + SSE events).
    incoming_rx: mpsc::Receiver<String>,
    /// Channel sender used by `send()` to queue POST response bodies
    /// and cloned for the SSE background task.
    incoming_tx: mpsc::Sender<String>,
    /// Handle to the SSE background task, if running.
    sse_task: Option<JoinHandle<()>>,
    /// The last SSE event ID received, for stream resumption.
    last_event_id: Arc<AtomicU64>,
    /// Whether the transport is still connected.
    connected: Arc<AtomicBool>,
    /// Configuration options.
    config: HttpClientConfig,
}

impl HttpClientTransport {
    /// Create a new HTTP client transport targeting the given URL.
    ///
    /// Uses default configuration. The URL should be the MCP server's
    /// Streamable HTTP endpoint (e.g., `http://localhost:3000`).
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::client::HttpClientTransport;
    ///
    /// let transport = HttpClientTransport::new("http://localhost:3000");
    /// ```
    pub fn new(url: impl Into<String>) -> Self {
        Self::with_config(url, HttpClientConfig::default())
    }

    /// Create with custom configuration.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::client::{HttpClientTransport, HttpClientConfig};
    /// use std::time::Duration;
    ///
    /// let config = HttpClientConfig {
    ///     request_timeout: Duration::from_secs(60),
    ///     sse_reconnect: false,
    ///     ..Default::default()
    /// };
    /// let transport = HttpClientTransport::with_config("http://localhost:3000", config);
    /// ```
    pub fn with_config(url: impl Into<String>, config: HttpClientConfig) -> Self {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        Self {
            url: url.into(),
            client: reqwest::Client::new(),
            session_id: None,
            protocol_version: None,
            incoming_rx: rx,
            incoming_tx: tx,
            sse_task: None,
            last_event_id: Arc::new(AtomicU64::new(0)),
            connected: Arc::new(AtomicBool::new(true)),
            config,
        }
    }

    /// Create with an existing `reqwest::Client`.
    ///
    /// Use this when you need custom TLS configuration, proxy settings,
    /// or connection pooling.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::client::HttpClientTransport;
    ///
    /// let client = reqwest::Client::builder()
    ///     .danger_accept_invalid_certs(true) // for development
    ///     .build()
    ///     .unwrap();
    /// let transport = HttpClientTransport::with_client("https://mcp.example.com", client);
    /// ```
    pub fn with_client(url: impl Into<String>, client: reqwest::Client) -> Self {
        let config = HttpClientConfig::default();
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        Self {
            url: url.into(),
            client,
            session_id: None,
            protocol_version: None,
            incoming_rx: rx,
            incoming_tx: tx,
            sse_task: None,
            last_event_id: Arc::new(AtomicU64::new(0)),
            connected: Arc::new(AtomicBool::new(true)),
            config,
        }
    }

    /// Start the SSE background stream after session is established.
    fn start_sse_stream(&mut self) {
        let url = self.url.clone();
        let client = self.client.clone();
        let session_id = self.session_id.clone().unwrap();
        let protocol_version = self.protocol_version.clone();
        let tx = self.incoming_tx.clone();
        let last_event_id = self.last_event_id.clone();
        let connected = self.connected.clone();
        let config = self.config.clone();

        self.sse_task = Some(tokio::spawn(async move {
            sse_stream_loop(SseLoopParams {
                url,
                client,
                session_id,
                protocol_version,
                tx,
                last_event_id,
                connected,
                config,
            })
            .await;
        }));
    }
}

#[async_trait]
impl ClientTransport for HttpClientTransport {
    async fn send(&mut self, message: &str) -> Result<()> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(Error::Transport("Transport closed".to_string()));
        }

        // Build request with headers
        let mut request = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .timeout(self.config.request_timeout);

        if let Some(ref session_id) = self.session_id {
            request = request.header("mcp-session-id", session_id);
        }

        if let Some(ref version) = self.protocol_version {
            request = request.header("mcp-protocol-version", version);
        }

        for (key, value) in &self.config.headers {
            request = request.header(key.as_str(), value.as_str());
        }

        let response = request
            .body(message.to_string())
            .send()
            .await
            .map_err(|e| Error::Transport(format!("HTTP request failed: {}", e)))?;

        let status = response.status();

        // Extract session headers before consuming the body
        let new_session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let new_protocol_version = response
            .headers()
            .get("mcp-protocol-version")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        // 202 Accepted = notification acknowledged, no body
        if status == reqwest::StatusCode::ACCEPTED {
            // Still update session state if headers present
            if let Some(sid) = new_session_id {
                self.session_id = Some(sid);
            }
            if let Some(pv) = new_protocol_version {
                self.protocol_version = Some(pv);
            }
            return Ok(());
        }

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Transport(format!(
                "HTTP {} from server: {}",
                status, body
            )));
        }

        // Update session state
        if let Some(sid) = new_session_id {
            let is_new_session = self.session_id.is_none();
            self.session_id = Some(sid);

            if is_new_session && self.config.auto_sse {
                self.start_sse_stream();
            }
        }
        if let Some(pv) = new_protocol_version {
            self.protocol_version = Some(pv);
        }

        // Read response body and queue for recv()
        let body = response
            .text()
            .await
            .map_err(|e| Error::Transport(format!("Failed to read response: {}", e)))?;

        if !body.is_empty() {
            self.incoming_tx
                .send(body)
                .await
                .map_err(|_| Error::Transport("Internal channel closed".to_string()))?;
        }

        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<String>> {
        match self.incoming_rx.recv().await {
            Some(msg) => Ok(Some(msg)),
            None => {
                self.connected.store(false, Ordering::Release);
                Ok(None)
            }
        }
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    async fn close(&mut self) -> Result<()> {
        self.connected.store(false, Ordering::Release);

        // Abort SSE task
        if let Some(task) = self.sse_task.take() {
            task.abort();
        }

        // Send DELETE to terminate the session (best effort)
        if let Some(ref session_id) = self.session_id {
            let _ = self
                .client
                .delete(&self.url)
                .header("mcp-session-id", session_id)
                .timeout(Duration::from_secs(5))
                .send()
                .await;
        }

        self.session_id = None;
        Ok(())
    }
}

// =============================================================================
// SSE Stream Background Loop
// =============================================================================

/// Parameters for the SSE background loop.
struct SseLoopParams {
    url: String,
    client: reqwest::Client,
    session_id: String,
    protocol_version: Option<String>,
    tx: mpsc::Sender<String>,
    last_event_id: Arc<AtomicU64>,
    connected: Arc<AtomicBool>,
    config: HttpClientConfig,
}

/// Background loop that maintains the SSE stream connection.
///
/// Opens a GET request with `Accept: text/event-stream` and parses
/// incoming SSE events. Events are pushed into the mpsc channel for
/// `recv()` to return. Supports reconnection with `Last-Event-ID`.
async fn sse_stream_loop(params: SseLoopParams) {
    let SseLoopParams {
        url,
        client,
        session_id,
        protocol_version,
        tx,
        last_event_id,
        connected,
        config,
    } = params;
    let mut reconnect_attempts = 0u32;

    loop {
        if !connected.load(Ordering::Acquire) {
            break;
        }

        let mut request = client
            .get(&url)
            .header("Accept", "text/event-stream")
            .header("mcp-session-id", &session_id);

        if let Some(ref version) = protocol_version {
            request = request.header("mcp-protocol-version", version);
        }

        for (key, value) in &config.headers {
            request = request.header(key.as_str(), value.as_str());
        }

        // Send Last-Event-ID for stream resumption
        let lei = last_event_id.load(Ordering::Acquire);
        if lei > 0 {
            request = request.header("Last-Event-ID", lei.to_string());
        }

        let response = match request.send().await {
            Ok(r) if r.status().is_success() => {
                reconnect_attempts = 0;
                r
            }
            Ok(r) => {
                tracing::warn!(status = %r.status(), "SSE connection rejected");
                break;
            }
            Err(e) => {
                tracing::warn!(error = %e, "SSE connection failed");
                if !config.sse_reconnect || reconnect_attempts >= config.max_sse_reconnect_attempts
                {
                    break;
                }
                reconnect_attempts += 1;
                tokio::time::sleep(config.sse_reconnect_delay).await;
                continue;
            }
        };

        // Parse SSE stream
        let mut stream = response.bytes_stream();
        let mut parser = SseParser::new();

        use futures::StreamExt;
        loop {
            match stream.next().await {
                Some(Ok(bytes)) => {
                    let text = String::from_utf8_lossy(&bytes);
                    for event in parser.feed(&text) {
                        if let Some(id) = event.id {
                            last_event_id.store(id, Ordering::Release);
                        }
                        if !event.data.is_empty() && tx.send(event.data).await.is_err() {
                            return; // Channel closed, transport dropped
                        }
                    }
                }
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "SSE stream error");
                    break;
                }
                None => {
                    tracing::debug!("SSE stream ended");
                    break;
                }
            }
        }

        // Attempt reconnection
        if !config.sse_reconnect
            || !connected.load(Ordering::Acquire)
            || reconnect_attempts >= config.max_sse_reconnect_attempts
        {
            break;
        }
        reconnect_attempts += 1;
        tracing::info!(
            attempt = reconnect_attempts,
            max = config.max_sse_reconnect_attempts,
            "Reconnecting SSE stream"
        );
        tokio::time::sleep(config.sse_reconnect_delay).await;
    }
}

// =============================================================================
// SSE Parser
// =============================================================================

/// A parsed SSE event.
#[derive(Debug)]
struct SseEvent {
    /// Event ID (from `id:` line), if present.
    id: Option<u64>,
    /// Event data (from `data:` lines, joined with newlines).
    data: String,
}

/// Incremental SSE parser.
///
/// Handles partial chunks from the byte stream, buffering incomplete
/// lines across `feed()` calls.
struct SseParser {
    /// Partial line buffer (when a chunk ends mid-line).
    buffer: String,
    /// Current event being parsed.
    current_id: Option<u64>,
    current_data: Vec<String>,
}

impl SseParser {
    fn new() -> Self {
        Self {
            buffer: String::new(),
            current_id: None,
            current_data: Vec::new(),
        }
    }

    /// Feed a chunk of text and return any complete events.
    fn feed(&mut self, text: &str) -> Vec<SseEvent> {
        self.buffer.push_str(text);
        let mut events = Vec::new();

        // Process complete lines
        while let Some(newline_pos) = self.buffer.find('\n') {
            let line = self.buffer[..newline_pos]
                .trim_end_matches('\r')
                .to_string();
            self.buffer = self.buffer[newline_pos + 1..].to_string();

            if line.is_empty() {
                // Empty line = end of event
                if !self.current_data.is_empty() {
                    events.push(SseEvent {
                        id: self.current_id.take(),
                        data: self.current_data.join("\n"),
                    });
                    self.current_data.clear();
                }
                self.current_id = None;
            } else if let Some(value) = line.strip_prefix("id:") {
                self.current_id = value.trim().parse().ok();
            } else if let Some(value) = line.strip_prefix("data:") {
                self.current_data.push(value.trim().to_string());
            }
            // Lines starting with ':' are comments (keep-alive) -- ignored
            // Lines starting with 'event:' are event types -- ignored (we only care about data)
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // SseParser tests
    // =========================================================================

    #[test]
    fn test_parse_complete_event() {
        let mut parser = SseParser::new();
        let events = parser.feed("id: 1\nevent: message\ndata: {\"hello\":\"world\"}\n\n");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, Some(1));
        assert_eq!(events[0].data, "{\"hello\":\"world\"}");
    }

    #[test]
    fn test_parse_multiple_events() {
        let mut parser = SseParser::new();
        let events =
            parser.feed("id: 1\ndata: first\n\nid: 2\ndata: second\n\nid: 3\ndata: third\n\n");

        assert_eq!(events.len(), 3);
        assert_eq!(events[0].data, "first");
        assert_eq!(events[1].data, "second");
        assert_eq!(events[2].data, "third");
        assert_eq!(events[0].id, Some(1));
        assert_eq!(events[1].id, Some(2));
        assert_eq!(events[2].id, Some(3));
    }

    #[test]
    fn test_parse_partial_chunks() {
        let mut parser = SseParser::new();

        // First chunk: partial event
        let events = parser.feed("id: 1\nda");
        assert!(events.is_empty());

        // Second chunk: completes the event
        let events = parser.feed("ta: hello\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, Some(1));
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn test_parse_multiline_data() {
        let mut parser = SseParser::new();
        let events = parser.feed("id: 1\ndata: line1\ndata: line2\ndata: line3\n\n");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2\nline3");
    }

    #[test]
    fn test_parse_comment_lines() {
        let mut parser = SseParser::new();
        let events = parser.feed(": keep-alive\nid: 1\ndata: hello\n\n");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn test_parse_event_without_id() {
        let mut parser = SseParser::new();
        let events = parser.feed("data: no-id-event\n\n");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, None);
        assert_eq!(events[0].data, "no-id-event");
    }

    #[test]
    fn test_empty_data_no_event() {
        let mut parser = SseParser::new();
        let events = parser.feed("id: 1\n\n");

        // No data lines = no event produced
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_crlf_line_endings() {
        let mut parser = SseParser::new();
        let events = parser.feed("id: 1\r\ndata: crlf\r\n\r\n");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "crlf");
    }

    #[test]
    fn test_parse_json_data() {
        let mut parser = SseParser::new();
        let json = r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"token":"t1","progress":50}}"#;
        let input = format!("id: 42\nevent: message\ndata: {}\n\n", json);
        let events = parser.feed(&input);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, Some(42));

        // Verify it's valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(parsed["method"], "notifications/progress");
    }

    // =========================================================================
    // Config tests
    // =========================================================================

    #[test]
    fn test_default_config() {
        let config = HttpClientConfig::default();
        assert!(config.auto_sse);
        assert_eq!(config.channel_capacity, 256);
        assert_eq!(config.request_timeout, Duration::from_secs(30));
        assert!(config.sse_reconnect);
        assert_eq!(config.sse_reconnect_delay, Duration::from_secs(1));
        assert_eq!(config.max_sse_reconnect_attempts, 5);
        assert!(config.headers.is_empty());
    }

    // =========================================================================
    // Transport constructor tests
    // =========================================================================

    #[test]
    fn test_new_transport() {
        let transport = HttpClientTransport::new("http://localhost:3000");
        assert_eq!(transport.url, "http://localhost:3000");
        assert!(transport.session_id.is_none());
        assert!(transport.protocol_version.is_none());
        assert!(transport.is_connected());
    }

    #[test]
    fn test_with_config() {
        let config = HttpClientConfig {
            request_timeout: Duration::from_secs(60),
            sse_reconnect: false,
            ..Default::default()
        };
        let transport = HttpClientTransport::with_config("http://example.com", config);
        assert_eq!(transport.url, "http://example.com");
        assert_eq!(transport.config.request_timeout, Duration::from_secs(60));
        assert!(!transport.config.sse_reconnect);
    }

    #[test]
    fn test_with_client() {
        let client = reqwest::Client::new();
        let transport = HttpClientTransport::with_client("http://example.com", client);
        assert_eq!(transport.url, "http://example.com");
        assert!(transport.is_connected());
    }
}
