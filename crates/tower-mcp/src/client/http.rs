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
use base64::Engine;
use tokio::sync::{Notify, RwLock, mpsc};
use tokio::task::JoinHandle;

use super::transport::ClientTransport;
use crate::error::{Error, Result};
use crate::protocol::{RequestId, notifications};

const MCP_METHOD_HEADER: &str = "mcp-method";
const MCP_NAME_HEADER: &str = "mcp-name";
const MCP_PARAM_HEADER_PREFIX: &str = "mcp-param-";
const BASE64_SENTINEL_PREFIX: &str = "=?base64?";
const BASE64_SENTINEL_SUFFIX: &str = "?=";

#[derive(Debug, Clone)]
struct CustomHeaderMapping {
    suffix: String,
    property_path: Vec<String>,
}

#[cfg(feature = "oauth-client")]
#[derive(Clone)]
struct ScopeEscalationRuntime {
    handler: Arc<dyn OAuthScopeEscalationHandler>,
    state: Arc<tokio::sync::Mutex<ScopeEscalationState>>,
    max_attempts: usize,
}

#[cfg(feature = "oauth-client")]
struct ScopeEscalationState {
    scopes: Vec<String>,
    revision: usize,
}

#[cfg(feature = "oauth-client")]
impl ScopeEscalationRuntime {
    fn new<P>(handler: Arc<P>, config: OAuthScopeEscalationConfig) -> Self
    where
        P: OAuthScopeEscalationHandler,
    {
        Self {
            handler,
            state: Arc::new(tokio::sync::Mutex::new(ScopeEscalationState {
                scopes: config.initial_scopes().to_vec(),
                revision: 0,
            })),
            max_attempts: config.maximum_attempts(),
        }
    }

    async fn respond_to_challenge(
        &self,
        challenge: OAuthScopeChallenge,
        resource: &str,
        operation: &str,
        attempt: usize,
        observed_revision: usize,
    ) -> std::result::Result<ScopeEscalationDecision, OAuthClientError> {
        // Serialize the complete reauthorization flow. A second operation
        // challenged by the same scope can reuse the token produced by the
        // first rather than opening a duplicate browser/headless flow.
        let mut state = self.state.lock().await;
        let previous_scopes = state.scopes.clone();
        let mut requested_scopes = previous_scopes.clone();
        for scope in &challenge.required_scopes {
            if !requested_scopes.contains(scope) {
                requested_scopes.push(scope.clone());
            }
        }

        if requested_scopes == previous_scopes && state.revision > observed_revision {
            return Ok(ScopeEscalationDecision {
                revision: state.revision,
            });
        }
        // The scope may have been requested previously without being granted,
        // or the token may otherwise be stale. Reauthorize the same union
        // again, but only within the caller's hard attempt limit.

        self.handler
            .reauthorize(OAuthScopeEscalationRequest {
                resource: resource.to_string(),
                operation: operation.to_string(),
                challenge,
                previous_scopes,
                requested_scopes: requested_scopes.clone(),
                attempt,
            })
            .await?;

        state.scopes = requested_scopes;
        state.revision += 1;
        Ok(ScopeEscalationDecision {
            revision: state.revision,
        })
    }
}

#[cfg(feature = "oauth-client")]
struct ScopeEscalationDecision {
    revision: usize,
}

#[cfg(feature = "oauth-client")]
use super::oauth::{
    OAuthClientError, OAuthScopeChallenge, OAuthScopeEscalationConfig, OAuthScopeEscalationHandler,
    OAuthScopeEscalationRequest, TokenProvider,
};

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
    /// Whether to automatically open the standalone SSE notification stream
    /// after initialization. Associated POST response streams used for
    /// sampling, elicitation, and roots requests work independently.
    /// Default: `true`.
    pub auto_sse: bool,
    /// Capacity of the internal message channel.
    /// Default: 256.
    pub channel_capacity: usize,
    /// Timeout for HTTP requests.
    /// Default: 30 seconds.
    pub request_timeout: Duration,
    /// Timeout for notification POSTs (frames without an `id`), capped at
    /// `request_timeout`.
    ///
    /// Notifications are awaited inline so `notifications/initialized` is
    /// ordered ahead of the first request, which blocks the client's message
    /// loop for the duration. This bounds that block independently of
    /// `request_timeout` so a server that stalls a notification's `202` cannot
    /// freeze the client for the full request timeout.
    /// Default: 5 seconds.
    pub notification_timeout: Duration,
    /// Whether to attempt SSE reconnection on disconnect.
    /// Default: `true`.
    pub sse_reconnect: bool,
    /// Delay before SSE reconnection attempts.
    /// Default: 1 second.
    pub sse_reconnect_delay: Duration,
    /// Maximum SSE reconnection attempts before giving up.
    /// Default: 5.
    pub max_sse_reconnect_attempts: u32,
    /// Whether to support automatic session recovery on expiry.
    /// When enabled, HTTP 404 responses (with a session ID attached) and
    /// JSON-RPC -32005 errors trigger re-initialization.
    /// Default: `true`.
    pub session_recovery: bool,
    /// Maximum size in bytes buffered for a single SSE event.
    ///
    /// A server that streams an event without ever terminating it would
    /// otherwise grow the parse buffer without bound. When a single
    /// event's buffered size exceeds this cap, the stream is terminated
    /// with [`Error::SseEventTooLarge`].
    /// Default: 16 MiB (matching rmcp).
    pub max_sse_event_size: usize,
}

/// Default maximum buffered size for a single SSE event (16 MiB, matching
/// rmcp). See [`HttpClientConfig::max_sse_event_size`].
pub const DEFAULT_MAX_SSE_EVENT_SIZE: usize = 16 * 1024 * 1024;

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            headers: HashMap::new(),
            auto_sse: true,
            channel_capacity: 256,
            request_timeout: Duration::from_secs(30),
            notification_timeout: Duration::from_secs(5),
            sse_reconnect: true,
            sse_reconnect_delay: Duration::from_secs(1),
            max_sse_reconnect_attempts: 5,
            session_recovery: true,
            max_sse_event_size: DEFAULT_MAX_SSE_EVENT_SIZE,
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
    /// Validated custom-header mappings learned from the latest tools/list.
    tool_header_mappings: HashMap<String, Vec<CustomHeaderMapping>>,
    /// Channel receiver for incoming messages (POST responses + SSE events).
    incoming_rx: mpsc::Receiver<String>,
    /// Channel sender used by `send()` to queue POST response bodies
    /// and cloned for the SSE background task.
    incoming_tx: mpsc::Sender<String>,
    /// Handle to the SSE background task, if running.
    sse_task: Option<JoinHandle<()>>,
    /// In-flight POST response streams, keyed by their JSON-RPC request ID.
    ///
    /// Final `subscriptions/listen` requests stay here until cancelled or the
    /// server closes them. Other completed tasks are pruned on subsequent
    /// sends.
    request_tasks: HashMap<RequestId, JoinHandle<()>>,
    /// The last SSE event ID received, for stream resumption.
    last_event_id: Arc<RwLock<Option<String>>>,
    /// Server-requested retry delay from SSE `retry:` field.
    sse_retry_delay: Arc<RwLock<Option<Duration>>>,
    /// Signal to tell the SSE loop to close its current stream and reconnect.
    sse_reconnect_signal: Arc<Notify>,
    /// Whether the transport is still connected.
    connected: Arc<AtomicBool>,
    /// Configuration options.
    config: HttpClientConfig,
    /// Dynamic token provider for OAuth or other token-based auth.
    #[cfg(feature = "oauth-client")]
    token_provider: Option<Arc<dyn TokenProvider>>,
    /// Runtime policy for insufficient-scope challenges.
    #[cfg(feature = "oauth-client")]
    scope_escalation: Option<ScopeEscalationRuntime>,
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
            tool_header_mappings: HashMap::new(),
            incoming_rx: rx,
            incoming_tx: tx,
            sse_task: None,
            request_tasks: HashMap::new(),
            last_event_id: Arc::new(RwLock::new(None)),
            sse_retry_delay: Arc::new(RwLock::new(None)),
            sse_reconnect_signal: Arc::new(Notify::new()),
            connected: Arc::new(AtomicBool::new(true)),
            config,
            #[cfg(feature = "oauth-client")]
            token_provider: None,
            #[cfg(feature = "oauth-client")]
            scope_escalation: None,
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
            tool_header_mappings: HashMap::new(),
            incoming_rx: rx,
            incoming_tx: tx,
            sse_task: None,
            request_tasks: HashMap::new(),
            last_event_id: Arc::new(RwLock::new(None)),
            sse_retry_delay: Arc::new(RwLock::new(None)),
            sse_reconnect_signal: Arc::new(Notify::new()),
            connected: Arc::new(AtomicBool::new(true)),
            config,
            #[cfg(feature = "oauth-client")]
            token_provider: None,
            #[cfg(feature = "oauth-client")]
            scope_escalation: None,
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

    /// Disable automatic session recovery.
    ///
    /// By default, if the server returns a session expired error (HTTP 404
    /// with session ID or JSON-RPC -32005), the client will automatically
    /// re-initialize and retry the failed operation. Call this to disable
    /// that behavior and surface the error to the caller instead.
    pub fn disable_session_recovery(mut self) -> Self {
        self.config.session_recovery = false;
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
        self.scope_escalation = None;
        self
    }

    /// Set a token provider with bounded runtime scope escalation.
    ///
    /// When an MCP operation receives an HTTP 403 Bearer challenge with
    /// `error="insufficient_scope"`, the transport unions the challenged
    /// scopes with the scopes already tracked by `config`, invokes
    /// [`OAuthScopeEscalationHandler::reauthorize`], asks the provider for a
    /// fresh token, and retries the same operation. Reauthorization is
    /// serialized across concurrent requests, and each operation is bounded
    /// by [`OAuthScopeEscalationConfig::maximum_attempts`].
    ///
    /// The provider and handler are the same value so the handler can update
    /// the token returned by [`TokenProvider::get_token`].
    #[cfg(feature = "oauth-client")]
    pub fn with_scope_aware_token_provider<P>(
        mut self,
        provider: P,
        config: OAuthScopeEscalationConfig,
    ) -> Self
    where
        P: TokenProvider + OAuthScopeEscalationHandler,
    {
        let provider = Arc::new(provider);
        self.token_provider = Some(provider.clone());
        self.scope_escalation = Some(ScopeEscalationRuntime::new(provider, config));
        self
    }

    fn outgoing_custom_headers(&self, parsed: &serde_json::Value) -> Vec<(String, String)> {
        if parsed.get("method").and_then(serde_json::Value::as_str) != Some("tools/call") {
            return Vec::new();
        }
        let Some(params) = parsed.get("params") else {
            return Vec::new();
        };
        let Some(name) = params.get("name").and_then(serde_json::Value::as_str) else {
            return Vec::new();
        };
        let Some(mappings) = self.tool_header_mappings.get(name) else {
            return Vec::new();
        };
        let arguments = params.get("arguments").unwrap_or(&serde_json::Value::Null);

        mappings
            .iter()
            .filter_map(|mapping| {
                let value = value_at_property_path(arguments, &mapping.property_path)?;
                if value.is_null() {
                    return None;
                }
                let rendered = json_value_to_header_string(value)?;
                Some((
                    format!("{MCP_PARAM_HEADER_PREFIX}{}", mapping.suffix),
                    encode_header_value(&rendered),
                ))
            })
            .collect()
    }

    fn normalize_incoming_message(&mut self, message: String) -> String {
        if self.protocol_version.as_deref() != Some(crate::protocol::EXPERIMENTAL_PROTOCOL_VERSION)
        {
            return message;
        }
        let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(&message) else {
            return message;
        };
        let Some(tools) = parsed
            .get_mut("result")
            .and_then(|result| result.get_mut("tools"))
            .and_then(serde_json::Value::as_array_mut)
        else {
            return message;
        };

        self.tool_header_mappings.clear();
        tools.retain(|tool| {
            let Some(name) = tool.get("name").and_then(serde_json::Value::as_str) else {
                return false;
            };
            let Some(schema) = tool.get("inputSchema") else {
                return false;
            };
            match custom_header_mappings(schema) {
                Ok(mappings) => {
                    self.tool_header_mappings.insert(name.to_string(), mappings);
                    true
                }
                Err(error) => {
                    tracing::warn!(tool = %name, %error, "Excluding tool with invalid x-mcp-header annotations");
                    false
                }
            }
        });

        parsed.to_string()
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
        let reconnect_signal = self.sse_reconnect_signal.clone();
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
                reconnect_signal,
                connected,
                config,
                #[cfg(feature = "oauth-client")]
                token_provider,
            })
            .await;
        }));
    }
}

fn custom_header_mappings(
    schema: &serde_json::Value,
) -> std::result::Result<Vec<CustomHeaderMapping>, String> {
    fn annotation_count(value: &serde_json::Value) -> usize {
        match value {
            serde_json::Value::Object(object) => {
                usize::from(object.contains_key("x-mcp-header"))
                    + object.values().map(annotation_count).sum::<usize>()
            }
            serde_json::Value::Array(values) => values.iter().map(annotation_count).sum::<usize>(),
            _ => 0,
        }
    }

    fn is_tchar(byte: u8) -> bool {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            )
    }

    fn primitive_header_type(schema: &serde_json::Value) -> bool {
        match schema.get("type") {
            Some(serde_json::Value::String(kind)) => {
                matches!(kind.as_str(), "string" | "number" | "integer" | "boolean")
            }
            Some(serde_json::Value::Array(kinds)) => {
                let mut primitive = false;
                for kind in kinds {
                    match kind.as_str() {
                        Some("string" | "number" | "integer" | "boolean") if !primitive => {
                            primitive = true;
                        }
                        Some("null") => {}
                        _ => return false,
                    }
                }
                primitive
            }
            _ => false,
        }
    }

    fn walk(
        schema: &serde_json::Value,
        path: &mut Vec<String>,
        seen: &mut std::collections::HashSet<String>,
        mappings: &mut Vec<CustomHeaderMapping>,
    ) -> std::result::Result<(), String> {
        let Some(properties) = schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
        else {
            return Ok(());
        };
        for (property_name, property_schema) in properties {
            path.push(property_name.clone());
            if let Some(annotation) = property_schema.get("x-mcp-header") {
                let suffix = annotation
                    .as_str()
                    .ok_or_else(|| format!("annotation at {} is not a string", path.join(".")))?;
                if suffix.is_empty() || !suffix.bytes().all(is_tchar) {
                    return Err(format!(
                        "invalid header suffix {suffix:?} at {}",
                        path.join(".")
                    ));
                }
                if !primitive_header_type(property_schema) {
                    return Err(format!(
                        "annotation at {} is not on a primitive property",
                        path.join(".")
                    ));
                }
                if !seen.insert(suffix.to_ascii_lowercase()) {
                    return Err(format!("duplicate header suffix {suffix:?}"));
                }
                mappings.push(CustomHeaderMapping {
                    suffix: suffix.to_string(),
                    property_path: path.clone(),
                });
            }
            walk(property_schema, path, seen, mappings)?;
            path.pop();
        }
        Ok(())
    }

    let mut mappings = Vec::new();
    walk(
        schema,
        &mut Vec::new(),
        &mut std::collections::HashSet::new(),
        &mut mappings,
    )?;
    if annotation_count(schema) != mappings.len() {
        return Err(
            "x-mcp-header annotation is not statically reachable through properties".to_string(),
        );
    }
    Ok(mappings)
}

fn value_at_property_path<'a>(
    root: &'a serde_json::Value,
    path: &[String],
) -> Option<&'a serde_json::Value> {
    path.iter().try_fold(root, |value, key| value.get(key))
}

fn json_value_to_header_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        serde_json::Value::Null | serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            None
        }
    }
}

fn encode_header_value(value: &str) -> String {
    let unsafe_for_header =
        value.trim() != value || value.bytes().any(|byte| !(0x20..=0x7e).contains(&byte));
    if unsafe_for_header {
        format!(
            "{BASE64_SENTINEL_PREFIX}{}{BASE64_SENTINEL_SUFFIX}",
            base64::engine::general_purpose::STANDARD.encode(value)
        )
    } else {
        value.to_string()
    }
}

#[cfg(feature = "oauth-client")]
fn bearer_headers(token: &str) -> std::result::Result<reqwest::header::HeaderMap, String> {
    let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| "token provider returned an invalid bearer token".to_string())?;
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::AUTHORIZATION, value);
    // RequestBuilder::header appends, which can leave a stale Authorization
    // value in front of the fresh token. Supplying a HeaderMap replaces the
    // existing value instead.
    Ok(headers)
}

fn is_jsonrpc_error_response(value: &serde_json::Value) -> bool {
    value.get("error").is_some_and(serde_json::Value::is_object)
        && value.pointer("/error/code").is_some()
        && value.pointer("/error/message").is_some()
}

struct HttpRequestSendError {
    message: String,
    connection_failed: bool,
}

impl HttpRequestSendError {
    fn request(error: reqwest::Error) -> Self {
        Self {
            message: format!("HTTP request failed: {error}"),
            connection_failed: true,
        }
    }

    #[cfg(feature = "oauth-client")]
    fn oauth(error: OAuthClientError) -> Self {
        Self {
            message: error.to_string(),
            connection_failed: false,
        }
    }
}

async fn send_http_request(
    mut request: reqwest::RequestBuilder,
    resource: &str,
    operation: &str,
    #[cfg(feature = "oauth-client")] token_provider: Option<Arc<dyn TokenProvider>>,
    #[cfg(feature = "oauth-client")] scope_escalation: Option<ScopeEscalationRuntime>,
    #[cfg(feature = "oauth-client")] initial_scope_revision: usize,
) -> std::result::Result<reqwest::Response, HttpRequestSendError> {
    #[cfg(not(feature = "oauth-client"))]
    let _ = (resource, operation);

    #[cfg(feature = "oauth-client")]
    let mut observed_revision = initial_scope_revision;
    #[cfg(feature = "oauth-client")]
    let mut attempts = 0;

    loop {
        #[cfg(feature = "oauth-client")]
        let retry_request = request.try_clone();

        let response = request
            .send()
            .await
            .map_err(HttpRequestSendError::request)?;

        #[cfg(feature = "oauth-client")]
        {
            let challenge = if response.status() == reqwest::StatusCode::FORBIDDEN {
                scope_challenge(response.headers())
            } else {
                None
            };
            let Some(challenge) = challenge else {
                return Ok(response);
            };
            let (Some(runtime), Some(provider), Some(mut retry_request)) = (
                scope_escalation.as_ref(),
                token_provider.as_ref(),
                retry_request,
            ) else {
                return Ok(response);
            };
            if attempts >= runtime.max_attempts {
                return Ok(response);
            }

            attempts += 1;
            let decision = runtime
                .respond_to_challenge(challenge, resource, operation, attempts, observed_revision)
                .await
                .map_err(HttpRequestSendError::oauth)?;
            observed_revision = decision.revision;

            let token = provider
                .get_token()
                .await
                .map_err(HttpRequestSendError::oauth)?;
            let headers = bearer_headers(&token).map_err(|message| {
                HttpRequestSendError::oauth(OAuthClientError::ScopeEscalation(message))
            })?;
            retry_request = retry_request.headers(headers);
            request = retry_request;
        }

        #[cfg(not(feature = "oauth-client"))]
        return Ok(response);
    }
}

#[cfg(feature = "oauth-client")]
fn scope_challenge(headers: &reqwest::header::HeaderMap) -> Option<OAuthScopeChallenge> {
    headers
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(OAuthScopeChallenge::from_www_authenticate)
}

fn http_status_error(status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap) -> String {
    #[cfg(feature = "oauth-client")]
    if let Some(challenge) = scope_challenge(headers) {
        let mut message = format!(
            "server returned HTTP {status}: insufficient_scope requires {}",
            challenge.required_scopes.join(" ")
        );
        if let Some(resource_metadata) = challenge.resource_metadata {
            message.push_str(&format!(" (resource metadata: {resource_metadata})"));
        }
        return message;
    }

    #[cfg(not(feature = "oauth-client"))]
    let _ = headers;
    format!("server returned HTTP {status}")
}

fn operation_label(parsed: Option<&serde_json::Value>) -> String {
    let Some(method) = parsed
        .and_then(|value| value.get("method"))
        .and_then(serde_json::Value::as_str)
    else {
        return "unknown".to_string();
    };
    let target = match method {
        "tools/call" | "prompts/get" => parsed
            .and_then(|value| value.pointer("/params/name"))
            .and_then(serde_json::Value::as_str),
        "resources/read" => parsed
            .and_then(|value| value.pointer("/params/uri"))
            .and_then(serde_json::Value::as_str),
        "tasks/get" | "tasks/update" | "tasks/cancel" => parsed
            .and_then(|value| value.pointer("/params/taskId"))
            .and_then(serde_json::Value::as_str),
        _ => None,
    };
    match target {
        Some(target) => format!("{method}:{target}"),
        None => method.to_string(),
    }
}

#[async_trait]
impl ClientTransport for HttpClientTransport {
    async fn send(&mut self, message: &str) -> Result<()> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(Error::Transport("Transport closed".to_string()));
        }

        // Notifications (frames without an `id`) are awaited inline (below) to
        // keep `notifications/initialized` ordered before the first request
        // (#967). That inline await blocks the whole message loop, so it must
        // be bounded independently: a server that stalls the notification's
        // 202 (observed: a multi-instance server holding the POST for the full
        // request timeout) would otherwise freeze the client with no output.
        let parsed_message = serde_json::from_str::<serde_json::Value>(message).ok();
        let is_notification = parsed_message
            .as_ref()
            .map(|v| v.get("id").is_none())
            .unwrap_or(false);
        let method = parsed_message
            .as_ref()
            .and_then(|value| value.get("method"))
            .and_then(serde_json::Value::as_str);
        let operation = operation_label(parsed_message.as_ref());
        let outbound_version = parsed_message
            .as_ref()
            .and_then(|value| {
                value.pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
            })
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let is_modern_request =
            outbound_version.as_deref() == Some(crate::protocol::EXPERIMENTAL_PROTOCOL_VERSION);
        if is_modern_request {
            self.protocol_version = outbound_version.clone();
            // Final requests are sessionless even when a transitional peer
            // incorrectly returns a legacy session header from discovery.
            self.session_id = None;
        }
        let timeout = if is_notification {
            self.config
                .notification_timeout
                .min(self.config.request_timeout)
        } else {
            self.config.request_timeout
        };

        // Build request with headers
        let mut request = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        // A subscription is intentionally long-lived; its handle owns the
        // timeout/cancellation decision. Every ordinary request remains
        // bounded by the configured request timeout.
        if method != Some("subscriptions/listen") {
            request = request.timeout(timeout);
        }

        if !is_modern_request && let Some(ref session_id) = self.session_id {
            request = request.header("mcp-session-id", session_id);
        }

        if let Some(version) = outbound_version.as_ref().or(self.protocol_version.as_ref()) {
            request = request.header("mcp-protocol-version", version);
        }

        if let Some(method) = method {
            request = request.header(MCP_METHOD_HEADER, method);
            let name = match method {
                "tools/call" | "prompts/get" => parsed_message
                    .as_ref()
                    .and_then(|value| value.pointer("/params/name"))
                    .and_then(serde_json::Value::as_str),
                "resources/read" => parsed_message
                    .as_ref()
                    .and_then(|value| value.pointer("/params/uri"))
                    .and_then(serde_json::Value::as_str),
                "tasks/get" | "tasks/update" | "tasks/cancel" => parsed_message
                    .as_ref()
                    .and_then(|value| value.pointer("/params/taskId"))
                    .and_then(serde_json::Value::as_str),
                _ => None,
            };
            if let Some(name) = name {
                request = request.header(MCP_NAME_HEADER, name);
            }
        }

        if let Some(parsed) = parsed_message.as_ref() {
            for (name, value) in self.outgoing_custom_headers(parsed) {
                request = request.header(name, value);
            }
        }

        for (key, value) in &self.config.headers {
            request = request.header(key.as_str(), value.as_str());
        }

        #[cfg(feature = "oauth-client")]
        let initial_scope_revision = match &self.scope_escalation {
            Some(runtime) => runtime.state.lock().await.revision,
            None => 0,
        };

        // Dynamic token provider overrides static Authorization header
        #[cfg(feature = "oauth-client")]
        if let Some(ref provider) = self.token_provider {
            let token = provider
                .get_token()
                .await
                .map_err(|e| Error::Transport(format!("Token provider error: {}", e)))?;
            request = request.headers(bearer_headers(&token).map_err(Error::Transport)?);
        }

        let request = request.body(message.to_string());

        // Notifications are awaited inline (bounded above) even after session
        // establishment, so `notifications/initialized` reaches the server
        // ahead of the first request rather than racing it on a pooled
        // connection, which strict servers rejected (#967).
        //
        // After session is established, send requests in a background task
        // so the message loop can continue processing incoming SSE messages.
        // This prevents a deadlock when the server blocks on a
        // bidirectional request (sampling/elicitation) that requires the
        // client to handle a request on the originating POST response stream.
        if !is_notification && (self.session_id.is_some() || is_modern_request) {
            let tx = self.incoming_tx.clone();
            // The caller is parked on this request id in the message loop's
            // correlation map. Every failure branch below delivers a frame
            // carrying it, so a background POST that dies (network error,
            // timeout, HTTP error, empty body, response stream closed early)
            // wakes the caller with an error instead of hanging it forever.
            let req_id = parsed_message
                .as_ref()
                .and_then(|value| value.get("id"))
                .cloned();
            let request_id = req_id
                .clone()
                .and_then(|value| serde_json::from_value(value).ok());
            let is_subscription = method == Some("subscriptions/listen");
            let connected = self.connected.clone();
            let last_event_id = self.last_event_id.clone();
            let sse_retry_delay = self.sse_retry_delay.clone();
            let sse_reconnect_signal = self.sse_reconnect_signal.clone();
            let max_sse_event_size = self.config.max_sse_event_size;
            let request_resource = self.url.clone();
            #[cfg(feature = "oauth-client")]
            let token_provider = self.token_provider.clone();
            #[cfg(feature = "oauth-client")]
            let scope_escalation = self.scope_escalation.clone();
            self.request_tasks.retain(|_, task| !task.is_finished());
            let task = tokio::spawn(async move {
                let response_result = send_http_request(
                    request,
                    &request_resource,
                    &operation,
                    #[cfg(feature = "oauth-client")]
                    token_provider,
                    #[cfg(feature = "oauth-client")]
                    scope_escalation,
                    #[cfg(feature = "oauth-client")]
                    initial_scope_revision,
                )
                .await;
                let response = match response_result {
                    Ok(r) => r,
                    Err(e) => {
                        let connection_failed = e.connection_failed;
                        tracing::error!(error = %e.message, "Background HTTP request failed");
                        if let Some(id) = &req_id {
                            let _ = tx.send(transport_error_frame(id, &e.message)).await;
                        }
                        if connection_failed {
                            connected.store(false, Ordering::Release);
                        }
                        return;
                    }
                };

                let status = response.status();

                // 202 Accepted = notification acknowledged, no body
                if status == reqwest::StatusCode::ACCEPTED {
                    return;
                }

                if !status.is_success() {
                    let status_error = http_status_error(status, response.headers());
                    let body = response.text().await.unwrap_or_default();

                    // Forward a JSON-RPC error body so the message loop can
                    // detect -32005 (SessionNotFound) and trigger session
                    // recovery. If the server did not echo our request id (a
                    // null/absent id that is not the session-level -32005
                    // signal), inject it so the awaiting caller is woken by
                    // this error instead of hanging.
                    if !body.is_empty()
                        && let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&body)
                        && is_jsonrpc_error_response(&v)
                    {
                        let is_session_signal =
                            v.pointer("/error/code").and_then(|c| c.as_i64()) == Some(-32005);
                        if !is_session_signal
                            && v.get("id").is_none_or(|id| id.is_null())
                            && let Some(id) = &req_id
                        {
                            v["id"] = id.clone();
                        }
                        let _ = tx.send(v.to_string()).await;
                        return;
                    }

                    tracing::error!(status = %status, body = %body, "HTTP error from server");
                    if let Some(id) = &req_id {
                        let _ = tx.send(transport_error_frame(id, &status_error)).await;
                    }
                    connected.store(false, Ordering::Release);
                    return;
                }

                // Check if response is SSE-formatted
                let is_sse = response
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|ct| ct.contains("text/event-stream"));

                if is_sse {
                    // Stream SSE response to extract id/retry fields
                    let mut stream = response.bytes_stream();
                    let mut parser = SseParser::with_limit(max_sse_event_size);
                    let mut had_retry = false;
                    let mut had_data = false;
                    let mut subscription_acknowledged = false;

                    use futures::StreamExt;
                    while let Some(result) = stream.next().await {
                        match result {
                            Ok(bytes) => {
                                let text = String::from_utf8_lossy(&bytes);
                                let events = match parser.feed(&text) {
                                    Ok(events) => events,
                                    Err(e) => {
                                        // A single event exceeded the cap;
                                        // terminate the stream instead of
                                        // buffering without bound. The
                                        // response for this request is lost,
                                        // so the transport is unusable.
                                        tracing::error!(error = %e, "POST SSE stream terminated");
                                        connected.store(false, Ordering::Release);
                                        return;
                                    }
                                };
                                for event in events {
                                    if let Some(ref id) = event.id {
                                        *last_event_id.write().await = Some(id.clone());
                                    }
                                    if let Some(retry_ms) = event.retry {
                                        *sse_retry_delay.write().await =
                                            Some(Duration::from_millis(retry_ms));
                                        had_retry = true;
                                    }
                                    if !event.data.is_empty() {
                                        had_data = true;
                                        let value =
                                            serde_json::from_str::<serde_json::Value>(&event.data);
                                        let value = match value {
                                            Ok(value) => value,
                                            Err(error) if is_subscription => {
                                                if let Some(id) = &req_id {
                                                    let _ = tx
                                                        .send(transport_error_frame(
                                                            id,
                                                            &format!(
                                                                "subscription stream returned invalid JSON: {error}"
                                                            ),
                                                        ))
                                                        .await;
                                                }
                                                return;
                                            }
                                            Err(_) => {
                                                let _ = tx.send(event.data).await;
                                                continue;
                                            }
                                        };
                                        let is_terminal =
                                            value.get("id").zip(req_id.as_ref()).is_some_and(
                                                |(actual, expected)| {
                                                    json_request_ids_match(actual, expected)
                                                },
                                            ) && (value.get("result").is_some()
                                                || value.get("error").is_some());

                                        if is_subscription {
                                            let violation = if is_terminal {
                                                if value.get("error").is_some() {
                                                    None
                                                } else if !subscription_acknowledged {
                                                    Some(
                                                        "subscriptions/listen completed before acknowledgment",
                                                    )
                                                } else if !value
                                                    .pointer(
                                                        "/result/_meta/io.modelcontextprotocol~1subscriptionId",
                                                    )
                                                    .zip(req_id.as_ref())
                                                    .is_some_and(|(actual, expected)| {
                                                        json_request_ids_match(actual, expected)
                                                    })
                                                {
                                                    Some(
                                                        "subscriptions/listen result carried a missing or mismatched subscription ID",
                                                    )
                                                } else {
                                                    None
                                                }
                                            } else if value.get("method").is_some()
                                                && value.get("id").is_none()
                                            {
                                                let correlated = value
                                                    .pointer(
                                                        "/params/_meta/io.modelcontextprotocol~1subscriptionId",
                                                    )
                                                    .zip(req_id.as_ref())
                                                    .is_some_and(|(actual, expected)| {
                                                        json_request_ids_match(actual, expected)
                                                    });
                                                let is_acknowledgment = value
                                                    .get("method")
                                                    .and_then(serde_json::Value::as_str)
                                                    == Some(
                                                        notifications::SUBSCRIPTIONS_ACKNOWLEDGED,
                                                    );
                                                if !correlated {
                                                    Some(
                                                        "subscription notification carried a missing or mismatched subscription ID",
                                                    )
                                                } else if !subscription_acknowledged
                                                    && !is_acknowledgment
                                                {
                                                    Some(
                                                        "subscription notification arrived before acknowledgment",
                                                    )
                                                } else if subscription_acknowledged
                                                    && is_acknowledgment
                                                {
                                                    Some(
                                                        "subscription stream sent a duplicate acknowledgment",
                                                    )
                                                } else {
                                                    if is_acknowledgment {
                                                        subscription_acknowledged = true;
                                                    }
                                                    None
                                                }
                                            } else {
                                                Some(
                                                    "subscription stream returned an unrelated JSON-RPC message",
                                                )
                                            };
                                            if let Some(message) = violation {
                                                if let Some(id) = &req_id {
                                                    let _ = tx
                                                        .send(transport_error_frame(id, message))
                                                        .await;
                                                }
                                                return;
                                            }
                                        }
                                        let _ = tx.send(event.data).await;
                                        if is_terminal {
                                            // The request is complete. Close the response
                                            // body ourselves even if a non-conforming server
                                            // leaves the SSE stream open after its final reply.
                                            return;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "POST SSE stream error");
                                break;
                            }
                        }
                    }

                    // If the POST SSE stream closed with a retry hint but no data,
                    // the server expects us to reconnect the GET notification stream.
                    // Signal the SSE loop to close its current stream and reconnect
                    // with the updated last_event_id and sse_retry_delay.
                    if !is_modern_request && had_retry && !had_data {
                        sse_reconnect_signal.notify_one();
                    } else {
                        // The response stream closed without ever delivering a
                        // terminal response. Acknowledgments and ordinary
                        // notifications do not complete a request, so wake the
                        // caller rather than leave it hanging.
                        if let Some(id) = &req_id {
                            let reason = if had_data {
                                "server closed the response stream before the final reply"
                            } else {
                                "server closed the response stream without a reply"
                            };
                            let _ = tx.send(transport_error_frame(id, reason)).await;
                        }
                    }
                } else {
                    // Non-SSE response: read body and queue for recv().
                    match response.text().await {
                        Ok(body) if !body.is_empty() => {
                            let msgs = extract_json_messages(&body);
                            if msgs.is_empty() {
                                // A non-empty body that yields no JSON-RPC
                                // frames leaves the request uncorrelated.
                                if let Some(id) = &req_id {
                                    let _ = tx
                                        .send(transport_error_frame(
                                            id,
                                            "server returned an unparseable response body",
                                        ))
                                        .await;
                                }
                            } else {
                                for msg in msgs {
                                    let _ = tx.send(msg).await;
                                }
                            }
                        }
                        Ok(_) => {
                            // 2xx with an empty body: no frame to correlate the
                            // request, so wake the caller rather than hang.
                            if let Some(id) = &req_id {
                                let _ = tx
                                    .send(transport_error_frame(
                                        id,
                                        "server returned an empty response body",
                                    ))
                                    .await;
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to read response body");
                            if let Some(id) = &req_id {
                                let _ = tx
                                    .send(transport_error_frame(
                                        id,
                                        &format!("failed to read response body: {e}"),
                                    ))
                                    .await;
                            }
                            connected.store(false, Ordering::Release);
                        }
                    }
                }
            });
            if let Some(request_id) = request_id {
                self.request_tasks.insert(request_id, task);
            }
            return Ok(());
        }

        // Pre-session (initialize) and notifications: handle synchronously.
        // For initialize this extracts session headers and starts the SSE
        // stream; for notifications the expected response is a bare 202.
        let response = send_http_request(
            request,
            &self.url,
            &operation,
            #[cfg(feature = "oauth-client")]
            self.token_provider.clone(),
            #[cfg(feature = "oauth-client")]
            self.scope_escalation.clone(),
            #[cfg(feature = "oauth-client")]
            initial_scope_revision,
        )
        .await
        .map_err(|e| Error::Transport(e.message))?;

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
            if !is_modern_request && let Some(sid) = new_session_id {
                self.session_id = Some(sid);
            }
            if let Some(pv) = new_protocol_version {
                self.protocol_version = Some(pv);
            }
            return Ok(());
        }

        if !status.is_success() {
            #[cfg(feature = "oauth-client")]
            let status_error = http_status_error(status, response.headers());
            let body = response.text().await.unwrap_or_default();
            if is_modern_request
                && let Ok(mut error) = serde_json::from_str::<serde_json::Value>(&body)
                && is_jsonrpc_error_response(&error)
            {
                if error.get("id").is_none_or(serde_json::Value::is_null)
                    && let Some(id) = parsed_message.as_ref().and_then(|value| value.get("id"))
                {
                    error["id"] = id.clone();
                }
                self.incoming_tx
                    .send(error.to_string())
                    .await
                    .map_err(|_| Error::Transport("Internal channel closed".to_string()))?;
                return Ok(());
            }
            // 404 only signals an expired session once a session exists.
            // Before that (the initial `initialize`), a 404 means the URL is
            // wrong, and reporting it as "Session expired" sends users
            // hunting session bugs instead of checking the endpoint path.
            if status == reqwest::StatusCode::NOT_FOUND
                && self.config.session_recovery
                && self.session_id.is_some()
            {
                return Err(Error::SessionExpired);
            }
            if status == reqwest::StatusCode::NOT_FOUND && self.session_id.is_none() {
                return Err(Error::Transport(format!(
                    "HTTP 404 from {}: MCP endpoint not found (check the endpoint path; \
                     some servers serve MCP at the root, others at /mcp)",
                    self.url
                )));
            }
            #[cfg(feature = "oauth-client")]
            if status == reqwest::StatusCode::FORBIDDEN
                && status_error.contains("insufficient_scope")
            {
                return Err(Error::Transport(if body.is_empty() {
                    status_error
                } else {
                    format!("{status_error}: {body}")
                }));
            }
            return Err(Error::Transport(format!(
                "HTTP {status} from server: {body}"
            )));
        }

        // Update session state
        if !is_modern_request && let Some(sid) = new_session_id {
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
            // All response paths converge here, including background final
            // POSTs. Normalize tools/list results on the transport-owning
            // task so validated x-mcp-header mappings are available to the
            // next tools/call without sharing mutable state across tasks.
            Some(msg) => Ok(Some(self.normalize_incoming_message(msg))),
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

        for (_, task) in self.request_tasks.drain() {
            task.abort();
        }

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
                && let Ok(headers) = bearer_headers(&token)
            {
                request = request.headers(headers);
            }

            let _ = request.send().await;
        }

        self.session_id = None;
        Ok(())
    }

    async fn reset_session(&mut self) {
        tracing::info!("Resetting session for re-initialization");

        for (_, task) in self.request_tasks.drain() {
            task.abort();
        }

        // Abort SSE task
        if let Some(task) = self.sse_task.take() {
            task.abort();
        }

        // Clear session state but keep the transport alive
        self.session_id = None;
        self.protocol_version = None;
        *self.last_event_id.write().await = None;
        *self.sse_retry_delay.write().await = None;

        // Drain any stale messages from the channel
        while self.incoming_rx.try_recv().is_ok() {}
    }

    fn supports_session_recovery(&self) -> bool {
        self.config.session_recovery
    }

    async fn cancel_request(&mut self, request_id: &RequestId) -> Result<()> {
        if let Some(task) = self.request_tasks.remove(request_id) {
            // Dropping reqwest's response byte stream closes this request's
            // HTTP response body, which is the final protocol's cancellation
            // signal. Other concurrent POST streams remain alive.
            task.abort();
            let _ = task.await;
        }
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
    reconnect_signal: Arc<Notify>,
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
        reconnect_signal,
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
                Ok(token) => match bearer_headers(&token) {
                    Ok(headers) => request = request.headers(headers),
                    Err(error) => {
                        tracing::warn!(%error, "Token provider failed for SSE connection");
                        break;
                    }
                },
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

        // Parse SSE stream, also listening for reconnect signals from POST handlers
        let mut stream = response.bytes_stream();
        let mut parser = SseParser::with_limit(config.max_sse_event_size);

        use futures::StreamExt;
        loop {
            tokio::select! {
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            let text = String::from_utf8_lossy(&bytes);
                            let events = match parser.feed(&text) {
                                Ok(events) => events,
                                Err(e) => {
                                    // A single event exceeded the cap;
                                    // terminate the stream (no reconnect,
                                    // the server would just repeat it)
                                    // instead of buffering without bound.
                                    tracing::error!(error = %e, "SSE stream terminated");
                                    connected.store(false, Ordering::Release);
                                    return;
                                }
                            };
                            for event in events {
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
                _ = reconnect_signal.notified() => {
                    tracing::debug!("SSE reconnect signal received, closing current stream");
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
/// Build a JSON-RPC error frame carrying `id`.
///
/// A post-session request POST is spawned in the background (so the message
/// loop can keep servicing the SSE stream), which means its failures happen
/// out of band from the caller. The caller is parked on the request id in the
/// message loop's correlation map; if the background POST fails before a real
/// response reaches the incoming channel, we must still deliver a frame with
/// this id or the caller hangs until the process exits. `-32000` is the
/// generic server-error code; the message names the transport-level cause.
fn transport_error_frame(id: &serde_json::Value, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32000, "message": message },
    })
    .to_string()
}

fn json_request_ids_match(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    left == right
        || match (left, right) {
            (serde_json::Value::Number(number), serde_json::Value::String(value))
            | (serde_json::Value::String(value), serde_json::Value::Number(number)) => number
                .as_i64()
                .is_some_and(|number| value.parse::<i64>() == Ok(number)),
            _ => false,
        }
}

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
        // The body is already fully in memory here, so no event-size cap
        // applies: an unlimited parser never returns an error.
        let mut parser = SseParser::new();
        let events = parser.feed(body).unwrap_or_default();
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
/// lines across `feed()` calls. When constructed with
/// [`with_limit`](Self::with_limit), a single event whose buffered size
/// exceeds the limit terminates parsing with
/// [`Error::SseEventTooLarge`] instead of growing without bound
/// (rmcp #970 analog).
struct SseParser {
    /// Partial line buffer (when a chunk ends mid-line).
    buffer: String,
    /// Current event being parsed.
    current_id: Option<String>,
    current_data: Vec<String>,
    current_retry: Option<u64>,
    /// Total bytes across `current_data` lines, tracked incrementally.
    data_len: usize,
    /// Maximum buffered size for a single event.
    max_event_size: usize,
}

impl SseParser {
    /// Create a parser with no event-size limit.
    fn new() -> Self {
        Self::with_limit(usize::MAX)
    }

    /// Create a parser that rejects events buffering more than
    /// `max_event_size` bytes.
    fn with_limit(max_event_size: usize) -> Self {
        Self {
            buffer: String::new(),
            current_id: None,
            current_data: Vec::new(),
            current_retry: None,
            data_len: 0,
            max_event_size,
        }
    }

    /// Feed a chunk of text and return any complete events.
    ///
    /// Returns [`Error::SseEventTooLarge`] when the bytes buffered for a
    /// single in-progress event exceed the configured limit. The parser
    /// should not be fed further after an error.
    fn feed(&mut self, text: &str) -> Result<Vec<SseEvent>> {
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
                    self.data_len = 0;
                }
                self.current_id = None;
                self.current_retry = None;
            } else if let Some(value) = line.strip_prefix("id:") {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    self.current_id = Some(trimmed.to_string());
                }
            } else if let Some(value) = line.strip_prefix("data:") {
                let data = value.trim().to_string();
                self.data_len += data.len();
                self.current_data.push(data);
            } else if let Some(value) = line.strip_prefix("retry:") {
                self.current_retry = value.trim().parse().ok();
            }
            // Lines starting with ':' are comments (keep-alive) -- ignored
            // Lines starting with 'event:' are event types -- ignored (we only care about data)
        }

        // Everything still buffered belongs to a single unfinished event
        // (or an unfinished line of one). Cap it so a server that never
        // terminates an event can't grow the buffers without bound.
        let buffered = self.buffer.len() + self.data_len;
        if buffered > self.max_event_size {
            return Err(Error::SseEventTooLarge {
                size: buffered,
                limit: self.max_event_size,
            });
        }

        Ok(events)
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
        let events = parser
            .feed("id: 1\nevent: message\ndata: {\"hello\":\"world\"}\n\n")
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, Some("1".to_string()));
        assert_eq!(events[0].data, "{\"hello\":\"world\"}");
    }

    #[test]
    fn test_parse_multiple_events() {
        let mut parser = SseParser::new();
        let events = parser
            .feed("id: 1\ndata: first\n\nid: 2\ndata: second\n\nid: 3\ndata: third\n\n")
            .unwrap();

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
        let events = parser.feed("id: 1\nda").unwrap();
        assert!(events.is_empty());

        // Second chunk: completes the event
        let events = parser.feed("ta: hello\n\n").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, Some("1".to_string()));
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn test_parse_multiline_data() {
        let mut parser = SseParser::new();
        let events = parser
            .feed("id: 1\ndata: line1\ndata: line2\ndata: line3\n\n")
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2\nline3");
    }

    #[test]
    fn test_parse_comment_lines() {
        let mut parser = SseParser::new();
        let events = parser.feed(": keep-alive\nid: 1\ndata: hello\n\n").unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn test_parse_event_without_id() {
        let mut parser = SseParser::new();
        let events = parser.feed("data: no-id-event\n\n").unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, None);
        assert_eq!(events[0].data, "no-id-event");
    }

    #[test]
    fn test_empty_data_no_event() {
        let mut parser = SseParser::new();
        let events = parser.feed("id: 1\n\n").unwrap();

        // No data lines = no event produced
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_crlf_line_endings() {
        let mut parser = SseParser::new();
        let events = parser.feed("id: 1\r\ndata: crlf\r\n\r\n").unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "crlf");
    }

    #[test]
    fn test_parse_json_data() {
        let mut parser = SseParser::new();
        let json = r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"token":"t1","progress":50}}"#;
        let input = format!("id: 42\nevent: message\ndata: {}\n\n", json);
        let events = parser.feed(&input).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, Some("42".to_string()));

        // Verify it's valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(parsed["method"], "notifications/progress");
    }

    #[test]
    fn test_event_exceeding_limit_is_rejected() {
        let mut parser = SseParser::with_limit(64);

        // An unterminated data line larger than the limit trips the cap.
        let big = "data: ".to_string() + &"x".repeat(128);
        let err = parser.feed(&big).unwrap_err();
        match err {
            Error::SseEventTooLarge { size, limit } => {
                assert!(size > 64, "size {} should exceed limit", size);
                assert_eq!(limit, 64);
            }
            other => panic!("expected SseEventTooLarge, got {:?}", other),
        }
    }

    #[test]
    fn test_accumulated_data_lines_count_toward_limit() {
        let mut parser = SseParser::with_limit(64);

        // Many complete data lines belonging to one unterminated event.
        let mut result = Ok(Vec::new());
        for _ in 0..10 {
            result = parser.feed("data: 0123456789\n");
            if result.is_err() {
                break;
            }
        }
        assert!(matches!(result, Err(Error::SseEventTooLarge { .. })));
    }

    #[test]
    fn test_events_within_limit_pass() {
        let mut parser = SseParser::with_limit(64);
        let events = parser.feed("data: hello\n\ndata: world\n\n").unwrap();
        assert_eq!(events.len(), 2);
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

    #[test]
    fn sep_2243_encodes_only_unsafe_values() {
        assert_eq!(encode_header_value("us west 1"), "us west 1");
        assert_eq!(encode_header_value(""), "");
        assert_eq!(encode_header_value(" padded "), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(
            encode_header_value("Hello, 世界"),
            "=?base64?SGVsbG8sIOS4lueVjA==?="
        );
    }

    #[test]
    fn oauth_error_body_is_not_misclassified_as_jsonrpc() {
        assert!(!is_jsonrpc_error_response(&serde_json::json!({
            "error": "insufficient_scope",
            "error_description": "Token has insufficient scope"
        })));
        assert!(is_jsonrpc_error_response(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {
                "code": -32022,
                "message": "Unsupported protocol version"
            }
        })));
    }

    #[test]
    fn sep_2243_validates_custom_header_annotations() {
        let mappings = custom_header_mappings(&serde_json::json!({
            "type": "object",
            "properties": {
                "region": {"type": "string", "x-mcp-header": "Region"},
                "priority": {"type": "integer", "x-mcp-header": "Priority"},
                "ratio": {"type": "number", "x-mcp-header": "Ratio"}
            }
        }))
        .unwrap();
        assert_eq!(mappings.len(), 3);

        for invalid in [
            serde_json::json!({
                "type": "object",
                "properties": {"value": {"type": "object", "x-mcp-header": "Value"}}
            }),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "a": {"type": "string", "x-mcp-header": "Region"},
                    "b": {"type": "string", "x-mcp-header": "region"}
                }
            }),
            serde_json::json!({
                "type": "object",
                "properties": {"value": {"type": "string", "x-mcp-header": "Bad Header"}}
            }),
        ] {
            assert!(custom_header_mappings(&invalid).is_err());
        }
    }

    #[test]
    fn sep_2243_filters_invalid_tools_and_caches_valid_mappings() {
        let mut transport = HttpClientTransport::new("http://localhost:3000");
        transport.protocol_version =
            Some(crate::protocol::EXPERIMENTAL_PROTOCOL_VERSION.to_string());
        let normalized = transport.normalize_incoming_message(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "tools": [
                        {
                            "name": "valid",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "region": {"type": "string", "x-mcp-header": "Region"}
                                }
                            }
                        },
                        {
                            "name": "invalid",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "value": {"type": "array", "x-mcp-header": "Value"}
                                }
                            }
                        }
                    ]
                }
            })
            .to_string(),
        );
        let parsed: serde_json::Value = serde_json::from_str(&normalized).unwrap();
        assert_eq!(parsed["result"]["tools"].as_array().unwrap().len(), 1);
        assert!(transport.tool_header_mappings.contains_key("valid"));
        assert!(!transport.tool_header_mappings.contains_key("invalid"));
    }
}
