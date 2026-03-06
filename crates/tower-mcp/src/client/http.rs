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
//!
//! # Authentication
//!
//! ```rust,no_run
//! use tower_mcp::client::{McpClient, HttpClientTransport};
//!
//! # async fn example() -> Result<(), tower_mcp::BoxError> {
//! // Bearer token
//! let transport = HttpClientTransport::new("http://localhost:3000")
//!     .bearer_token("sk-your-token-here");
//!
//! // Custom API key header
//! let transport = HttpClientTransport::new("http://localhost:3000")
//!     .api_key_header("X-API-Key", "your-key");
//!
//! // Basic auth
//! let transport = HttpClientTransport::new("http://localhost:3000")
//!     .basic_auth("user", "password");
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;

use super::transport::ClientTransport;
use crate::error::{Error, Result};

#[cfg(feature = "oauth-client")]
use super::oauth::TokenProvider;

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

impl HttpClientConfig {
    /// Set a Bearer token for authentication.
    pub fn bearer_token(mut self, token: impl Into<String>) -> Self {
        self.headers.insert(
            "Authorization".to_string(),
            format!("Bearer {}", token.into()),
        );
        self
    }

    /// Set an API key using a custom header name.
    pub fn api_key_header(mut self, name: impl Into<String>, key: impl Into<String>) -> Self {
        self.headers.insert(name.into(), key.into());
        self
    }

    /// Set Basic authentication credentials.
    pub fn basic_auth(mut self, username: impl AsRef<str>, password: impl AsRef<str>) -> Self {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!(
            "{}:{}",
            username.as_ref(),
            password.as_ref()
        ));
        self.headers
            .insert("Authorization".to_string(), format!("Basic {}", encoded));
        self
    }

    /// Add a custom header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
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
    last_event_id: Arc<RwLock<Option<String>>>,
    /// Server-requested retry delay from SSE `retry:` field.
    sse_retry_delay: Arc<RwLock<Option<Duration>>>,
    /// Whether the transport is still connected.
    connected: Arc<AtomicBool>,
    /// Configuration options.
    config: HttpClientConfig,
    /// Dynamic token provider for OAuth or other token-based auth.
    #[cfg(feature = "oauth-client")]
    token_provider: Option<Arc<dyn TokenProvider>>,
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
            last_event_id: Arc::new(RwLock::new(None)),
            sse_retry_delay: Arc::new(RwLock::new(None)),
            connected: Arc::new(AtomicBool::new(true)),
            config,
            #[cfg(feature = "oauth-client")]
            token_provider: None,
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
            last_event_id: Arc::new(RwLock::new(None)),
            sse_retry_delay: Arc::new(RwLock::new(None)),
            connected: Arc::new(AtomicBool::new(true)),
            config,
            #[cfg(feature = "oauth-client")]
            token_provider: None,
        }
    }

    /// Set a Bearer token for `Authorization: Bearer <token>` authentication.
    ///
    /// The token is included on every HTTP request (POST and SSE GET).
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::client::HttpClientTransport;
    ///
    /// let transport = HttpClientTransport::new("http://localhost:3000")
    ///     .bearer_token("sk-my-secret-token");
    /// ```
    pub fn bearer_token(mut self, token: impl Into<String>) -> Self {
        self.config.headers.insert(
            "Authorization".to_string(),
            format!("Bearer {}", token.into()),
        );
        self
    }

    /// Set an API key for authentication.
    ///
    /// Sends as `Authorization: Bearer <key>`. Use
    /// [`api_key_header`](Self::api_key_header) for a custom header name.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::client::HttpClientTransport;
    ///
    /// let transport = HttpClientTransport::new("http://localhost:3000")
    ///     .api_key("sk-my-api-key");
    /// ```
    pub fn api_key(self, key: impl Into<String>) -> Self {
        self.bearer_token(key)
    }

    /// Set an API key using a custom header name.
    ///
    /// Sends the key as the raw header value (no `Bearer` prefix).
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::client::HttpClientTransport;
    ///
    /// let transport = HttpClientTransport::new("http://localhost:3000")
    ///     .api_key_header("X-API-Key", "sk-my-api-key");
    /// ```
    pub fn api_key_header(mut self, name: impl Into<String>, key: impl Into<String>) -> Self {
        self.config.headers.insert(name.into(), key.into());
        self
    }

    /// Set Basic authentication credentials.
    ///
    /// Encodes `username:password` as Base64 and sends as
    /// `Authorization: Basic <encoded>`.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::client::HttpClientTransport;
    ///
    /// let transport = HttpClientTransport::new("http://localhost:3000")
    ///     .basic_auth("admin", "secret");
    /// ```
    pub fn basic_auth(mut self, username: impl AsRef<str>, password: impl AsRef<str>) -> Self {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!(
            "{}:{}",
            username.as_ref(),
            password.as_ref()
        ));
        self.config
            .headers
            .insert("Authorization".to_string(), format!("Basic {}", encoded));
        self
    }

    /// Add a custom header to every request.
    ///
    /// Can be called multiple times to add multiple headers.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::client::HttpClientTransport;
    ///
    /// let transport = HttpClientTransport::new("http://localhost:3000")
    ///     .header("X-Custom-Header", "my-value")
    ///     .header("X-Request-Source", "my-app");
    /// ```
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.headers.insert(name.into(), value.into());
        self
    }

    /// Set a dynamic token provider for authentication.
    ///
    /// The provider's [`TokenProvider::get_token()`] is called before each
    /// HTTP request, and the returned token is sent as `Authorization: Bearer <token>`.
    /// This overrides any static `Authorization` header set via [`bearer_token()`](Self::bearer_token)
    /// or [`basic_auth()`](Self::basic_auth).
    ///
    /// Use [`OAuthClientCredentials`](super::OAuthClientCredentials) for
    /// OAuth 2.0 Client Credentials grants, or implement [`TokenProvider`]
    /// for custom token acquisition logic.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::client::{HttpClientTransport, OAuthClientCredentials};
    ///
    /// # fn example() -> Result<(), tower_mcp::BoxError> {
    /// let provider = OAuthClientCredentials::builder()
    ///     .client_id("my-client")
    ///     .client_secret("my-secret")
    ///     .token_endpoint("https://auth.example.com/token")
    ///     .build()?;
    ///
    /// let transport = HttpClientTransport::new("http://localhost:3000")
    ///     .with_token_provider(provider);
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "oauth-client")]
    pub fn with_token_provider(mut self, provider: impl TokenProvider) -> Self {
        self.token_provider = Some(Arc::new(provider));
        self
    }

    /// Start the SSE background stream after session is established.
    fn start_sse_stream(&mut self) {
        let url = self.url.clone();
        let client = self.client.clone();
        let session_id = self.session_id.clone().unwrap();
        let protocol_version = self.protocol_version.clone();
        let tx = self.incoming_tx.clone();
        let last_event_id = self.last_event_id.clone();
        let sse_retry_delay = self.sse_retry_delay.clone();
        let connected = self.connected.clone();
        let config = self.config.clone();
        #[cfg(feature = "oauth-client")]
        let token_provider = self.token_provider.clone();

        self.sse_task = Some(tokio::spawn(async move {
            sse_stream_loop(SseLoopParams {
                url,
                client,
                session_id,
                protocol_version,
                tx,
                last_event_id,
                sse_retry_delay,
                connected,
                config,
                #[cfg(feature = "oauth-client")]
                token_provider,
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

        // Dynamic token provider overrides static Authorization header
        #[cfg(feature = "oauth-client")]
        if let Some(ref provider) = self.token_provider {
            let token = provider
                .get_token()
                .await
                .map_err(|e| Error::Transport(format!("Token provider error: {}", e)))?;
            request = request.header("Authorization", format!("Bearer {}", token));
        }

        let request = request.body(message.to_string());

        // After session is established, send in a background task so the
        // message loop can continue processing incoming SSE messages.
        // This prevents a deadlock when the server blocks on a
        // bidirectional request (sampling/elicitation) that requires the
        // client to respond via the SSE channel.
        if self.session_id.is_some() {
            let tx = self.incoming_tx.clone();
            let connected = self.connected.clone();
            tokio::spawn(async move {
                let response = match request.send().await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!(error = %e, "Background HTTP request failed");
                        connected.store(false, Ordering::Release);
                        return;
                    }
                };

                let status = response.status();

                // 202 Accepted = notification acknowledged, no body
                if status == reqwest::StatusCode::ACCEPTED {
                    return;
                }

                if !status.is_success() {
                    let body = response.text().await.unwrap_or_default();
                    tracing::error!(status = %status, body = %body, "HTTP error from server");
                    connected.store(false, Ordering::Release);
                    return;
                }

                // Read response body and queue for recv()
                match response.text().await {
                    Ok(body) if !body.is_empty() => {
                        for msg in extract_json_messages(&body) {
                            let _ = tx.send(msg).await;
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to read response body");
                        connected.store(false, Ordering::Release);
                    }
                    _ => {}
                }
            });
            return Ok(());
        }

        // Pre-session (initialize): handle synchronously to extract
        // session headers and start the SSE stream.
        let response = request
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

        for msg in extract_json_messages(&body) {
            self.incoming_tx
                .send(msg)
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
            let mut request = self
                .client
                .delete(&self.url)
                .header("mcp-session-id", session_id)
                .timeout(Duration::from_secs(5));

            for (key, value) in &self.config.headers {
                request = request.header(key.as_str(), value.as_str());
            }

            // Dynamic token provider overrides static Authorization header
            #[cfg(feature = "oauth-client")]
            if let Some(ref provider) = self.token_provider
                && let Ok(token) = provider.get_token().await
            {
                request = request.header("Authorization", format!("Bearer {}", token));
            }

            let _ = request.send().await;
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
    last_event_id: Arc<RwLock<Option<String>>>,
    sse_retry_delay: Arc<RwLock<Option<Duration>>>,
    connected: Arc<AtomicBool>,
    config: HttpClientConfig,
    #[cfg(feature = "oauth-client")]
    token_provider: Option<Arc<dyn TokenProvider>>,
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
        sse_retry_delay,
        connected,
        config,
        #[cfg(feature = "oauth-client")]
        token_provider,
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

        // Dynamic token provider overrides static Authorization header
        #[cfg(feature = "oauth-client")]
        if let Some(ref provider) = token_provider {
            match provider.get_token().await {
                Ok(token) => {
                    request = request.header("Authorization", format!("Bearer {}", token));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Token provider failed for SSE connection");
                    break;
                }
            }
        }

        // Send Last-Event-ID for stream resumption
        if let Some(ref lei) = *last_event_id.read().await {
            request = request.header("Last-Event-ID", lei.clone());
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
                let delay = sse_retry_delay
                    .read()
                    .await
                    .unwrap_or(config.sse_reconnect_delay);
                tokio::time::sleep(delay).await;
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
                        if let Some(ref id) = event.id {
                            *last_event_id.write().await = Some(id.clone());
                        }
                        if let Some(retry_ms) = event.retry {
                            *sse_retry_delay.write().await = Some(Duration::from_millis(retry_ms));
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
        let delay = sse_retry_delay
            .read()
            .await
            .unwrap_or(config.sse_reconnect_delay);
        tracing::info!(
            attempt = reconnect_attempts,
            max = config.max_sse_reconnect_attempts,
            delay_ms = delay.as_millis() as u64,
            "Reconnecting SSE stream"
        );
        tokio::time::sleep(delay).await;
    }
}

// =============================================================================
// SSE Parser
// =============================================================================

/// Extract JSON messages from a response body.
///
/// If the body is SSE-formatted (`event: message\ndata: ...\n\n`), extracts the
/// `data:` content from each event. Otherwise returns the body as-is.
fn extract_json_messages(body: &str) -> Vec<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    // Heuristic: SSE bodies start with "event:" or "data:" or "id:" or ":"
    let looks_like_sse = trimmed.starts_with("event:")
        || trimmed.starts_with("data:")
        || trimmed.starts_with("id:")
        || trimmed.starts_with(':');

    if looks_like_sse {
        let mut parser = SseParser::new();
        let events = parser.feed(body);
        events.into_iter().map(|e| e.data).collect()
    } else {
        vec![trimmed.to_string()]
    }
}

/// A parsed SSE event.
#[derive(Debug)]
struct SseEvent {
    /// Event ID (from `id:` line), if present. String per SSE spec.
    id: Option<String>,
    /// Event data (from `data:` lines, joined with newlines).
    data: String,
    /// Server-requested retry delay in milliseconds (from `retry:` line).
    retry: Option<u64>,
}

/// Incremental SSE parser.
///
/// Handles partial chunks from the byte stream, buffering incomplete
/// lines across `feed()` calls.
struct SseParser {
    /// Partial line buffer (when a chunk ends mid-line).
    buffer: String,
    /// Current event being parsed.
    current_id: Option<String>,
    current_data: Vec<String>,
    current_retry: Option<u64>,
}

impl SseParser {
    fn new() -> Self {
        Self {
            buffer: String::new(),
            current_id: None,
            current_data: Vec::new(),
            current_retry: None,
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
                if !self.current_data.is_empty() || self.current_retry.is_some() {
                    events.push(SseEvent {
                        id: self.current_id.take(),
                        data: self.current_data.join("\n"),
                        retry: self.current_retry.take(),
                    });
                    self.current_data.clear();
                }
                self.current_id = None;
                self.current_retry = None;
            } else if let Some(value) = line.strip_prefix("id:") {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    self.current_id = Some(trimmed.to_string());
                }
            } else if let Some(value) = line.strip_prefix("data:") {
                self.current_data.push(value.trim().to_string());
            } else if let Some(value) = line.strip_prefix("retry:") {
                self.current_retry = value.trim().parse().ok();
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
        assert_eq!(events[0].id, Some("1".to_string()));
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
        assert_eq!(events[0].id, Some("1".to_string()));
        assert_eq!(events[1].id, Some("2".to_string()));
        assert_eq!(events[2].id, Some("3".to_string()));
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
        assert_eq!(events[0].id, Some("1".to_string()));
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
        assert_eq!(events[0].id, Some("42".to_string()));

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

    // =========================================================================
    // Auth builder tests
    // =========================================================================

    #[test]
    fn test_bearer_token() {
        let transport =
            HttpClientTransport::new("http://localhost:3000").bearer_token("sk-test-token");
        assert_eq!(
            transport.config.headers.get("Authorization").unwrap(),
            "Bearer sk-test-token"
        );
    }

    #[test]
    fn test_api_key() {
        let transport = HttpClientTransport::new("http://localhost:3000").api_key("sk-api-key-123");
        assert_eq!(
            transport.config.headers.get("Authorization").unwrap(),
            "Bearer sk-api-key-123"
        );
    }

    #[test]
    fn test_api_key_header() {
        let transport =
            HttpClientTransport::new("http://localhost:3000").api_key_header("X-API-Key", "my-key");
        assert_eq!(transport.config.headers.get("X-API-Key").unwrap(), "my-key");
        assert!(!transport.config.headers.contains_key("Authorization"));
    }

    #[test]
    fn test_basic_auth() {
        let transport =
            HttpClientTransport::new("http://localhost:3000").basic_auth("admin", "secret");
        let header = transport.config.headers.get("Authorization").unwrap();
        assert!(header.starts_with("Basic "));
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(header.strip_prefix("Basic ").unwrap())
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "admin:secret");
    }

    #[test]
    fn test_custom_header() {
        let transport = HttpClientTransport::new("http://localhost:3000")
            .header("X-Custom", "value1")
            .header("X-Another", "value2");
        assert_eq!(transport.config.headers.get("X-Custom").unwrap(), "value1");
        assert_eq!(transport.config.headers.get("X-Another").unwrap(), "value2");
    }

    #[test]
    fn test_chaining_with_config() {
        let config = HttpClientConfig {
            request_timeout: Duration::from_secs(60),
            ..Default::default()
        };
        let transport =
            HttpClientTransport::with_config("http://localhost:3000", config).bearer_token("tk");
        assert_eq!(transport.config.request_timeout, Duration::from_secs(60));
        assert_eq!(
            transport.config.headers.get("Authorization").unwrap(),
            "Bearer tk"
        );
    }

    #[test]
    fn test_last_auth_wins() {
        let transport = HttpClientTransport::new("http://localhost:3000")
            .bearer_token("token1")
            .basic_auth("user", "pass");
        let header = transport.config.headers.get("Authorization").unwrap();
        assert!(header.starts_with("Basic "));
    }

    #[test]
    fn test_config_bearer_token() {
        let config = HttpClientConfig::default().bearer_token("tk-123");
        assert_eq!(
            config.headers.get("Authorization").unwrap(),
            "Bearer tk-123"
        );
    }

    #[test]
    fn test_config_header() {
        let config = HttpClientConfig::default().header("X-Foo", "bar");
        assert_eq!(config.headers.get("X-Foo").unwrap(), "bar");
    }

    #[test]
    fn test_config_api_key_header() {
        let config = HttpClientConfig::default().api_key_header("X-Key", "secret");
        assert_eq!(config.headers.get("X-Key").unwrap(), "secret");
    }

    #[test]
    fn test_config_basic_auth() {
        let config = HttpClientConfig::default().basic_auth("user", "pw");
        let header = config.headers.get("Authorization").unwrap();
        assert!(header.starts_with("Basic "));
    }
}
