//! MCP Client with bidirectional communication support.
//!
//! Provides [`McpClient`] for connecting to MCP servers over any
//! [`ClientTransport`]. The client runs a background message loop that
//! handles request/response correlation, server-initiated requests
//! (sampling, elicitation, roots), and notifications.
//!
//! See [`crate::guides::client`] for transport selection, lifecycle setup,
//! callbacks, common requests, caching, retry policy, and shutdown guidance.
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::client::{McpClient, StdioClientTransport};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), tower_mcp::BoxError> {
//!     let transport = StdioClientTransport::spawn("my-mcp-server", &["--flag"]).await?;
//!     let client = McpClient::connect(transport).await?;
//!
//!     let server_info = client.initialize("my-client", "1.0.0").await?;
//!     println!("Connected to: {}", server_info.server_info.name);
//!
//!     let tools = client.list_tools().await?;
//!     for tool in &tools.tools {
//!         println!("Tool: {}", tool.name);
//!     }
//!
//!     let result = client.call_tool("my-tool", serde_json::json!({"arg": "value"})).await?;
//!     println!("Result: {:?}", result);
//!
//!     Ok(())
//! }
//! ```

mod channel;
mod handler;
#[cfg(feature = "http-client")]
mod http;
#[cfg(feature = "oauth-client")]
mod oauth;
#[cfg(feature = "oauth-client")]
mod oauth_authcode;
#[cfg(feature = "oauth-client")]
mod oauth_flow;
mod response_cache;
mod stdio;
mod transport;

pub use channel::ChannelTransport;
pub use handler::{ClientHandler, NotificationHandler, ServerNotification};
#[cfg(feature = "http-client")]
pub use http::{HttpClientConfig, HttpClientTransport};
#[cfg(feature = "oauth-client")]
pub use oauth::{
    OAuthBearerChallenge, OAuthClientCredentials, OAuthClientCredentialsBuilder, OAuthClientError,
    OAuthScopeChallenge, OAuthScopeEscalationConfig, OAuthScopeEscalationHandler,
    OAuthScopeEscalationRequest, OAuthTokenEndpointAuthMethod, TokenProvider,
};
#[cfg(feature = "oauth-client")]
pub use oauth_authcode::{
    MemoryOAuthClientRegistrationStore, OAuthApplicationType, OAuthAuthCodeConfig,
    OAuthAuthorizationCode, OAuthAuthorizationDiscovery, OAuthAuthorizationServerMetadata,
    OAuthClientRegistration, OAuthClientRegistrationMethod, OAuthClientRegistrationOptions,
    OAuthClientRegistrationStore, OAuthDynamicClientRegistration, OAuthProtectedResourceMetadata,
    discover_oauth_authorization, discover_oauth_authorization_server,
    probe_oauth_bearer_challenge, resolve_oauth_client_registration,
    resolve_oauth_client_registration_with_store,
};
#[cfg(feature = "oauth-client")]
pub use oauth_flow::{
    MemoryOAuthAuthorizationStateStore, MemoryOAuthTokenStore, OAuthAuthorizationAction,
    OAuthAuthorizationFlow, OAuthAuthorizationFlowBuilder, OAuthAuthorizationHandler,
    OAuthAuthorizationRequest, OAuthAuthorizationStart, OAuthAuthorizationStateStore,
    OAuthClientAssertionRequest, OAuthClientAssertionSigner, OAuthHttpBody, OAuthHttpClient,
    OAuthHttpMethod, OAuthHttpRequest, OAuthHttpResponse, OAuthPendingAuthorization,
    OAuthPendingAuthorizationState, OAuthRedirectPolicy, OAuthStoredToken, OAuthTokenBinding,
    OAuthTokenStore, ReqwestOAuthHttpClient,
};
pub use response_cache::{ClientCacheConfig, DEFAULT_MAX_CACHE_TTL};
pub use stdio::StdioClientTransport;
pub use transport::ClientTransport;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::ProtocolSupport;
use crate::error::{Error, ErrorCode, McpErrorCode, Result};
#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
use crate::protocol::DiscoverParams;
use crate::protocol::{
    CacheScope, CallToolParams, CallToolResult, CancelTaskParams, CancelledParams,
    ClientCapabilities, CompleteParams, CompleteResult, CompletionArgument, CompletionReference,
    CreateTaskResult, DiscoverResult, ElicitationCapability, GetPromptParams, GetPromptResult,
    GetTaskInfoParams, Implementation, InitializeParams, InitializeResult, InputRequest,
    InputRequests, InputResponse, InputResponses, JsonRpcNotification, JsonRpcRequest,
    ListPromptsParams, ListPromptsResult, ListResourceTemplatesParams, ListResourceTemplatesResult,
    ListResourcesParams, ListResourcesResult, ListRootsResult, ListToolsParams, ListToolsResult,
    PromptDefinition, ReadResourceParams, ReadResourceResult, RequestId, RequestMeta,
    RequestOutcome, ResourceDefinition, ResourceTemplateDefinition, Root, RootsCapability,
    SamplingCapability, SubscriptionFilter, SubscriptionsAcknowledgedParams,
    SubscriptionsListenParams, SubscriptionsListenResult, TaskObject, TaskRequestParams,
    TaskStatusParams, ToolDefinition, UpdateTaskParams, notifications,
};
use response_cache::{CacheLookup, ClientResponseCache};
use tower_mcp_types::JsonRpcError;

/// One response to a final-protocol `tools/call` request.
///
/// Task creation is server-directed in SEP-2663, so a client that declares
/// the Tasks extension must be prepared for an ordinary tool call to return a
/// task handle. [`McpClient::call_tool`] drives that task transparently;
/// [`McpClient::call_tool_once_task_aware`] exposes this enum for callers that
/// want direct control of the lifecycle.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum TaskAwareCallToolOutcome {
    /// The server elected to create a task.
    Task(crate::tasks::CreateTaskResult),
    /// The request completed synchronously.
    Complete(CallToolResult),
    /// The request needs one or more client inputs before it can complete.
    InputRequired(crate::protocol::InputRequiredResult),
}

trait CacheableResponse:
    Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static
{
    fn ttl_ms(&self) -> Option<u64>;
    fn cache_scope(&self) -> Option<CacheScope>;
}

macro_rules! impl_cacheable_response {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl CacheableResponse for $ty {
                fn ttl_ms(&self) -> Option<u64> {
                    self.ttl_ms
                }

                fn cache_scope(&self) -> Option<CacheScope> {
                    self.cache_scope
                }
            }
        )+
    };
}

impl_cacheable_response!(
    DiscoverResult,
    ListToolsResult,
    ListResourcesResult,
    ListResourceTemplatesResult,
    ListPromptsResult,
    ReadResourceResult,
);

/// Internal command sent from McpClient methods to the background loop.
enum LoopCommand {
    /// Send a JSON-RPC request and await a response.
    Request {
        method: String,
        params: serde_json::Value,
        response_tx: oneshot::Sender<Result<serde_json::Value>>,
    },
    /// Open a long-lived `subscriptions/listen` request and return its ID.
    StartSubscription {
        params: serde_json::Value,
        id_tx: oneshot::Sender<RequestId>,
        acknowledgment_tx: oneshot::Sender<SubscriptionFilter>,
        response_tx: oneshot::Sender<Result<serde_json::Value>>,
    },
    /// Cancel one active subscription request.
    CancelRequest {
        request_id: RequestId,
        done_tx: Option<oneshot::Sender<Result<()>>>,
    },
    /// Send a JSON-RPC notification (no JSON-RPC response expected; the
    /// completion channel reports whether the transport delivered it).
    Notify {
        method: String,
        params: serde_json::Value,
        done_tx: oneshot::Sender<Result<()>>,
    },
    /// Reset the transport's session state for re-initialization.
    ResetSession { done_tx: oneshot::Sender<()> },
    /// Fulfil embedded MRTR requests through the configured client handler.
    ResolveInputs {
        requests: InputRequests,
        response_tx: oneshot::Sender<Result<InputResponses>>,
    },
    /// Graceful shutdown.
    Shutdown,
}

/// An active `subscriptions/listen` request.
///
/// The handle exposes the JSON-RPC request ID used as the subscription ID,
/// the server's acknowledged filter, graceful server completion, and explicit
/// cancellation. Dropping an active handle requests cancellation on a
/// best-effort basis; callers that need confirmation should call
/// [`cancel()`](Self::cancel).
#[must_use = "dropping the handle cancels the active subscription"]
pub struct SubscriptionHandle {
    request_id: RequestId,
    command_tx: mpsc::Sender<LoopCommand>,
    acknowledgment_rx: Option<oneshot::Receiver<SubscriptionFilter>>,
    response_rx: Option<oneshot::Receiver<Result<serde_json::Value>>>,
    active: bool,
}

impl SubscriptionHandle {
    /// The JSON-RPC request ID that identifies this subscription.
    pub fn id(&self) -> &RequestId {
        &self.request_id
    }

    /// Wait for the server's mandatory first-message acknowledgment.
    ///
    /// The returned filter is the subset the server agreed to honor.
    pub async fn acknowledged(&mut self) -> Result<SubscriptionFilter> {
        let receiver = self.acknowledgment_rx.take().ok_or_else(|| {
            Error::Transport("subscription acknowledgment was already consumed".to_string())
        })?;
        receiver
            .await
            .map_err(|_| Error::Transport("subscription ended before acknowledgment".to_string()))
    }

    /// Wait for the server to end the subscription gracefully.
    ///
    /// An HTTP disconnect without a terminal response is reported as a
    /// transport error. Dropping this future drops the handle and cancels the
    /// subscription.
    pub async fn wait(mut self) -> Result<SubscriptionsListenResult> {
        let receiver = self.response_rx.take().ok_or_else(|| {
            Error::Transport("subscription result was already consumed".to_string())
        })?;
        let value = receiver
            .await
            .map_err(|_| Error::Transport("connection closed".to_string()))??;
        self.active = false;
        let result: SubscriptionsListenResult = serde_json::from_value(value).map_err(|error| {
            Error::Transport(format!(
                "failed to deserialize subscriptions/listen response: {error}"
            ))
        })?;
        if !result.result_type.is_complete() {
            return Err(Error::Transport(format!(
                "subscriptions/listen ended with unexpected result type {:?}",
                result.result_type
            )));
        }
        if !request_ids_match(&result.meta.subscription_id, &self.request_id) {
            return Err(Error::Transport(
                "subscriptions/listen result carried the wrong subscription ID".to_string(),
            ));
        }
        Ok(result)
    }

    /// Cancel the subscription and wait until its transport stream is closed.
    pub async fn cancel(mut self) -> Result<()> {
        let (done_tx, done_rx) = oneshot::channel();
        self.command_tx
            .send(LoopCommand::CancelRequest {
                request_id: self.request_id.clone(),
                done_tx: Some(done_tx),
            })
            .await
            .map_err(|_| Error::Transport("connection closed".to_string()))?;
        let result = done_rx
            .await
            .map_err(|_| Error::Transport("connection closed".to_string()))?;
        self.active = false;
        result
    }
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        if self.active {
            let _ = self.command_tx.try_send(LoopCommand::CancelRequest {
                request_id: self.request_id.clone(),
                done_tx: None,
            });
        }
    }
}

/// MCP client with a background message loop.
///
/// Unlike previous versions, this type is not generic over the transport.
/// The transport is consumed during [`connect()`](Self::connect) and moved
/// into a background Tokio task that handles message multiplexing.
///
/// All public methods take `&self`, enabling concurrent use from multiple
/// tasks.
///
/// # Construction
///
/// ```rust,no_run
/// use tower_mcp::client::{McpClient, StdioClientTransport};
///
/// # async fn example() -> Result<(), tower_mcp::BoxError> {
/// // Simple: no handler for server-initiated requests
/// let transport = StdioClientTransport::spawn("server", &[]).await?;
/// let client = McpClient::connect(transport).await?;
///
/// // With configuration
/// use tower_mcp::protocol::Root;
/// let transport = StdioClientTransport::spawn("server", &[]).await?;
/// let client = McpClient::builder()
///     .with_roots(vec![Root::new("file:///project")])
///     .connect_simple(transport)
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct McpClient {
    /// Channel to send commands to the background loop.
    command_tx: mpsc::Sender<LoopCommand>,
    /// Background task handle.
    task: Option<JoinHandle<()>>,
    /// Whether `initialize()` has been called successfully.
    initialized: AtomicBool,
    /// Server info (set after successful initialization).
    server_info: RwLock<Option<InitializeResult>>,
    /// Client capabilities declared during initialization.
    capabilities: ClientCapabilities,
    /// Exact ordered set of protocol implementations enabled for this client.
    protocol_support: ProtocolSupport,
    /// Protocol selected by the discover-based final lifecycle.
    selected_protocol_version: RwLock<Option<String>>,
    /// Client identity repeated in final-protocol request metadata.
    client_info: RwLock<Option<Implementation>>,
    /// Server discovery result, when the final lifecycle is active.
    discovery: RwLock<Option<DiscoverResult>>,
    /// Current roots (shared with the loop for roots/list responses).
    roots: Arc<RwLock<Vec<Root>>>,
    /// Whether the transport is still connected.
    connected: Arc<AtomicBool>,
    /// Whether the transport supports session recovery.
    supports_session_recovery: bool,
    /// Stored init params for session recovery re-initialization.
    init_params: RwLock<Option<(String, String)>>,
    /// Lock to prevent concurrent session recovery attempts.
    recovery_lock: Mutex<()>,
    /// Maximum number of input-required rounds auto-driven per operation.
    max_mrtr_rounds: usize,
    /// SEP-2549 final-protocol response cache.
    response_cache: Arc<ClientResponseCache>,
    /// Allocator for per-request progress tokens; `None` when the client did
    /// not opt into progress.
    progress_tokens: Option<Arc<AtomicI64>>,
}

/// Settings a builder hands to the connect path, grouped so the private
/// constructor keeps one parameter per concern rather than one per field.
struct ClientSettings {
    capabilities: ClientCapabilities,
    roots: Vec<Root>,
    protocol_support: ProtocolSupport,
    max_mrtr_rounds: usize,
    cache_config: ClientCacheConfig,
    request_progress: bool,
}

/// Builder for configuring and connecting an [`McpClient`].
///
/// # Example
///
/// ```rust,no_run
/// use tower_mcp::client::{McpClient, StdioClientTransport};
/// use tower_mcp::protocol::Root;
///
/// # async fn example() -> Result<(), tower_mcp::BoxError> {
/// let transport = StdioClientTransport::spawn("server", &[]).await?;
/// let handler = (); // Use a real ClientHandler for bidirectional support
/// let client = McpClient::builder()
///     .with_roots(vec![Root::new("file:///project")])
///     .with_sampling()
///     .connect(transport, handler)
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct McpClientBuilder {
    capabilities: ClientCapabilities,
    roots: Vec<Root>,
    protocol_support: ProtocolSupport,
    max_mrtr_rounds: usize,
    cache_config: ClientCacheConfig,
    request_progress: bool,
}

impl McpClientBuilder {
    /// Create a new builder with default settings.
    pub fn new() -> Self {
        Self {
            capabilities: ClientCapabilities::default(),
            roots: Vec::new(),
            // Every compiled implementation, matching servers (#1179). The
            // entry point still selects the era: nothing calls `discover`
            // implicitly, so compiling a feature never changes an existing
            // client's wire behavior, it only removes the configuration step
            // before `discover`.
            protocol_support: ProtocolSupport::default(),
            max_mrtr_rounds: 8,
            cache_config: ClientCacheConfig::default(),
            request_progress: false,
        }
    }

    /// Ask servers to report progress for this client's requests.
    ///
    /// A server only emits progress when the request carries a progress
    /// token, so [`RequestContext::report_progress`] is a no-op for a client
    /// that never sends one. With this enabled, every request this client
    /// issues carries a fresh token and the matching
    /// `notifications/progress` frames reach the handler's
    /// [`on_progress`](crate::client::NotificationHandler::on_progress)
    /// callback.
    ///
    /// Off by default: a token asks the server to do extra work, and a client
    /// with no progress handler would only add wire traffic.
    ///
    /// [`RequestContext::report_progress`]: crate::context::RequestContext::report_progress
    pub fn request_progress(mut self) -> Self {
        self.request_progress = true;
        self
    }

    /// Configure roots for this client.
    ///
    /// The client will declare roots support during initialization and
    /// respond to `roots/list` requests with these roots.
    pub fn with_roots(mut self, roots: Vec<Root>) -> Self {
        self.roots = roots;
        self.capabilities.roots = Some(RootsCapability {
            list_changed: true,
            deprecated: None,
        });
        self
    }

    /// Configure custom capabilities for this client.
    pub fn with_capabilities(mut self, capabilities: ClientCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Add one validated MCP protocol-extension declaration.
    ///
    /// Repeated declarations for the same identifier use last-write-wins
    /// semantics. Other configured capabilities are preserved.
    pub fn with_protocol_extension(mut self, extension: crate::ExtensionDeclaration) -> Self {
        let (identifier, settings) = extension.into_parts();
        self.capabilities
            .extensions
            .get_or_insert_default()
            .insert(identifier, settings);
        self
    }

    /// Set the exact ordered protocol versions enabled for this client.
    ///
    /// The default is [`ProtocolSupport::default`], every implementation
    /// compiled into this build: with `protocol-2026-07-28` enabled the
    /// client can call `McpClient::discover` without further configuration,
    /// and without the feature only the stable session protocols exist.
    /// Pass [`ProtocolSupport::stable`] to keep a feature-enabled build on
    /// the session protocols only.
    pub fn protocol_support(mut self, support: ProtocolSupport) -> Self {
        self.protocol_support = support;
        self
    }

    /// Bound the number of MRTR rounds automatically followed for one request.
    ///
    /// Zero is normalized to one. The default is eight rounds.
    pub fn max_mrtr_rounds(mut self, rounds: usize) -> Self {
        self.max_mrtr_rounds = rounds.max(1);
        self
    }

    /// Configure the SEP-2549 final-protocol response cache.
    pub fn response_cache(mut self, config: ClientCacheConfig) -> Self {
        self.cache_config = config;
        self
    }

    /// Disable the SEP-2549 response cache.
    pub fn disable_response_cache(mut self) -> Self {
        self.cache_config.enabled = false;
        self
    }

    /// Declare sampling support.
    ///
    /// Sets the sampling capability so the server knows this client can
    /// handle `sampling/createMessage` requests. The handler passed to
    /// [`connect()`](Self::connect) should override
    /// [`handle_create_message()`](ClientHandler::handle_create_message).
    pub fn with_sampling(mut self) -> Self {
        self.capabilities.sampling = Some(SamplingCapability::default());
        self
    }

    /// Declare elicitation support.
    ///
    /// Sets the elicitation capability so the server knows this client can
    /// handle `elicitation/create` requests. The handler passed to
    /// [`connect()`](Self::connect) should override
    /// [`handle_elicit()`](ClientHandler::handle_elicit).
    pub fn with_elicitation(mut self) -> Self {
        self.capabilities.elicitation = Some(ElicitationCapability::default());
        self
    }

    /// Connect to a server using the given transport and handler.
    ///
    /// Spawns a background task to handle message I/O. The transport is
    /// consumed and owned by the background task.
    pub async fn connect<T, H>(self, transport: T, handler: H) -> Result<McpClient>
    where
        T: ClientTransport,
        H: ClientHandler,
    {
        McpClient::connect_inner(
            transport,
            handler,
            ClientSettings {
                capabilities: self.capabilities,
                roots: self.roots,
                protocol_support: self.protocol_support,
                max_mrtr_rounds: self.max_mrtr_rounds,
                cache_config: self.cache_config,
                request_progress: self.request_progress,
            },
        )
        .await
    }

    /// Connect to a server without a handler.
    ///
    /// All server-initiated requests will be rejected with `method_not_found`.
    pub async fn connect_simple<T: ClientTransport>(self, transport: T) -> Result<McpClient> {
        self.connect(transport, ()).await
    }
}

impl Default for McpClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl McpClient {
    /// Connect with default settings and no handler.
    ///
    /// Shorthand for `McpClient::builder().connect_simple(transport)`.
    pub async fn connect<T: ClientTransport>(transport: T) -> Result<Self> {
        McpClientBuilder::new().connect_simple(transport).await
    }

    /// Connect with a handler for server-initiated requests.
    pub async fn connect_with_handler<T, H>(transport: T, handler: H) -> Result<Self>
    where
        T: ClientTransport,
        H: ClientHandler,
    {
        McpClientBuilder::new().connect(transport, handler).await
    }

    /// Create a builder for advanced configuration.
    pub fn builder() -> McpClientBuilder {
        McpClientBuilder::new()
    }

    /// Internal connect implementation.
    async fn connect_inner<T, H>(transport: T, handler: H, settings: ClientSettings) -> Result<Self>
    where
        T: ClientTransport,
        H: ClientHandler,
    {
        let ClientSettings {
            capabilities,
            roots,
            protocol_support,
            max_mrtr_rounds,
            cache_config,
            request_progress,
        } = settings;
        let supports_session_recovery = transport.supports_session_recovery();
        let (command_tx, command_rx) = mpsc::channel::<LoopCommand>(64);
        let connected = Arc::new(AtomicBool::new(true));
        let roots = Arc::new(RwLock::new(roots));
        let response_cache = ClientResponseCache::new(cache_config);

        let loop_connected = connected.clone();
        let loop_roots = roots.clone();
        let loop_response_cache = response_cache.clone();

        let task = tokio::spawn(async move {
            message_loop(
                transport,
                handler,
                command_rx,
                loop_connected,
                loop_roots,
                loop_response_cache,
            )
            .await;
        });

        Ok(Self {
            command_tx,
            task: Some(task),
            initialized: AtomicBool::new(false),
            server_info: RwLock::new(None),
            capabilities,
            protocol_support,
            selected_protocol_version: RwLock::new(None),
            client_info: RwLock::new(None),
            discovery: RwLock::new(None),
            roots,
            connected,
            supports_session_recovery,
            init_params: RwLock::new(None),
            recovery_lock: Mutex::new(()),
            max_mrtr_rounds,
            response_cache,
            progress_tokens: request_progress.then(|| Arc::new(AtomicI64::new(1))),
        })
    }

    /// Check if the client has been initialized.
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Check if the transport is still connected.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// Get the server info (available after initialization).
    pub async fn server_info(&self) -> Option<InitializeResult> {
        self.server_info.read().await.clone()
    }

    /// Return the exact ordered protocol implementations enabled for this client.
    pub fn protocol_support(&self) -> &ProtocolSupport {
        &self.protocol_support
    }

    /// Clear every cached final-protocol response held by this client.
    pub async fn clear_response_cache(&self) {
        self.response_cache.clear().await;
    }

    /// Change the authorization-context partition used for private responses.
    ///
    /// Previously cached private entries become inaccessible, while public
    /// entries remain reusable. Call this before issuing requests after the
    /// authenticated principal changes.
    pub async fn set_cache_partition(&self, partition: impl Into<String>) {
        self.response_cache.set_partition(partition.into()).await;
    }

    /// Return the number of response-cache entries held by this client.
    pub async fn response_cache_len(&self) -> usize {
        self.response_cache.len().await
    }

    /// Get the server discovery result after the final lifecycle is active.
    pub async fn discovery(&self) -> Option<DiscoverResult> {
        self.discovery.read().await.clone()
    }

    /// Get the protocol version selected for the discover-based lifecycle.
    pub async fn selected_protocol_version(&self) -> Option<String> {
        self.selected_protocol_version.read().await.clone()
    }

    /// Get the server info synchronously (best-effort, non-blocking).
    ///
    /// Returns `None` if the lock is currently held by a writer or if
    /// initialization hasn't completed. Prefer [`server_info()`](Self::server_info)
    /// in async contexts.
    pub fn server_info_blocking(&self) -> Option<InitializeResult> {
        self.server_info.try_read().ok()?.clone()
    }

    /// Initialize the MCP connection.
    ///
    /// Sends the `initialize` request and `notifications/initialized` notification.
    /// Must be called before any other operations.
    pub async fn initialize(
        &self,
        client_name: &str,
        client_version: &str,
    ) -> Result<InitializeResult> {
        let params = InitializeParams {
            protocol_version: crate::protocol::LATEST_PROTOCOL_VERSION.to_string(),
            capabilities: self.capabilities.clone(),
            client_info: Implementation {
                name: client_name.to_string(),
                version: client_version.to_string(),
                ..Default::default()
            },
            meta: None,
        };

        let result: InitializeResult = self.send_request("initialize", &params).await?;
        *self.server_info.write().await = Some(result.clone());

        // Store init params for potential session recovery
        *self.init_params.write().await =
            Some((client_name.to_string(), client_version.to_string()));

        // Send initialized notification. A delivery failure is an
        // initialization failure: the server will reject every subsequent
        // request until the notification arrives.
        self.send_notification("notifications/initialized", &serde_json::json!({}))
            .await
            .map_err(|error| {
                Error::Transport(format!(
                    "failed to deliver notifications/initialized: {error}"
                ))
            })?;
        self.initialized.store(true, Ordering::Release);

        Ok(result)
    }

    /// Start the sessionless 2026-07-28 lifecycle with `server/discover`.
    ///
    /// This path is available only when the final implementation was compiled.
    /// The client sends required per-request metadata from the first request,
    /// retries one `Unsupported protocol version` response using the server's
    /// advertised intersection, and then repeats the selected version,
    /// capabilities, and client identity on every subsequent request.
    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    pub async fn discover(
        &self,
        client_name: &str,
        client_version: &str,
    ) -> Result<DiscoverResult> {
        use crate::protocol::PROTOCOL_VERSION_2026_07_28;

        let client_info = Implementation {
            name: client_name.to_string(),
            version: client_version.to_string(),
            ..Default::default()
        };
        *self.client_info.write().await = Some(client_info.clone());

        let mut candidate = self
            .protocol_support
            .versions()
            .iter()
            .find(|version| version.as_str() == PROTOCOL_VERSION_2026_07_28)
            .cloned()
            .ok_or_else(|| {
                Error::Transport(
                    "2026-07-28 is not enabled for this client; configure ProtocolSupport"
                        .to_string(),
                )
            })?;
        let mut retried_unsupported = false;

        loop {
            let params = DiscoverParams {
                meta: Some(self.request_meta_for(&candidate, &client_info)),
            };
            let cache_key = serde_json::to_string(&(
                client_name,
                client_version,
                candidate.as_str(),
                &self.capabilities,
            ))
            .expect("discovery cache key is serializable");
            match self
                .send_cacheable_request_when::<_, DiscoverResult>(
                    "server/discover",
                    &cache_key,
                    &params,
                    true,
                )
                .await
            {
                Ok(result) => {
                    let selected = self
                        .protocol_support
                        .versions()
                        .iter()
                        .find(|version| {
                            result
                                .supported_versions
                                .iter()
                                .any(|supported| supported == *version)
                        })
                        .cloned()
                        .ok_or_else(|| {
                            Error::Transport(format!(
                                "server and client have no protocol version in common; server: {:?}, client: {:?}",
                                result.supported_versions,
                                self.protocol_support.versions()
                            ))
                        })?;
                    *self.selected_protocol_version.write().await = Some(selected);
                    *self.discovery.write().await = Some(result.clone());
                    self.initialized.store(true, Ordering::Release);
                    return Ok(result);
                }
                Err(Error::JsonRpc(error)) if error.code == -32022 && !retried_unsupported => {
                    let supported = error
                        .data
                        .as_ref()
                        .and_then(|data| data.get("supported"))
                        .and_then(serde_json::Value::as_array)
                        .ok_or_else(|| Error::JsonRpc(error.clone()))?;
                    candidate = self
                        .protocol_support
                        .versions()
                        .iter()
                        .find(|version| {
                            supported
                                .iter()
                                .any(|item| item.as_str() == Some(version.as_str()))
                        })
                        .cloned()
                        .ok_or_else(|| Error::JsonRpc(error.clone()))?;
                    retried_unsupported = true;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// List available tools.
    pub async fn list_tools(&self) -> Result<ListToolsResult> {
        self.list_tools_with_cursor(None).await
    }

    /// Call a tool.
    ///
    /// On the final lifecycle, a header mismatch, method-not-found, or
    /// invalid-params response can indicate that the cached tool schema is
    /// stale. The client invalidates `tools/list`, refreshes it, and retries
    /// the rejected round once. These errors are raised before tool execution,
    /// so the bounded retry does not replay a completed side effect.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult> {
        let mut input_responses = None;
        let mut request_state = None;
        let mut schema_retry_available = self.uses_final_protocol().await;
        for round in 0..=self.max_mrtr_rounds {
            let params = CallToolParams {
                name: name.to_string(),
                arguments: arguments.clone(),
                input_responses: input_responses.take(),
                request_state: request_state.take(),
                meta: None,
                task: None,
            };
            let outcome = self
                .send_task_aware_tool_request_with_schema_retry(
                    &params,
                    &mut schema_retry_available,
                )
                .await?;
            match outcome {
                TaskAwareCallToolOutcome::Complete(result) => return Ok(result),
                TaskAwareCallToolOutcome::Task(created) => {
                    return self
                        .complete_final_task(&created.task.metadata.task_id)
                        .await;
                }
                TaskAwareCallToolOutcome::InputRequired(required) => {
                    if round == self.max_mrtr_rounds {
                        return Err(Error::Transport(format!(
                            "MRTR round limit ({}) exceeded for tools/call",
                            self.max_mrtr_rounds
                        )));
                    }
                    let requests = required.input_requests.ok_or_else(|| {
                        Error::Transport(
                            "input_required result has no requests the client can fulfil"
                                .to_string(),
                        )
                    })?;
                    input_responses = Some(self.resolve_input_requests(requests).await?);
                    request_state = required.request_state;
                }
            }
        }
        unreachable!("MRTR loop either completes or returns at the configured bound")
    }

    /// Send one tools/call attempt without automatically following MRTR input.
    pub async fn call_tool_once(
        &self,
        name: &str,
        arguments: serde_json::Value,
        input_responses: Option<InputResponses>,
        request_state: Option<String>,
    ) -> Result<RequestOutcome<CallToolResult>> {
        match self
            .call_tool_once_task_aware(name, arguments, input_responses, request_state)
            .await?
        {
            TaskAwareCallToolOutcome::Complete(result) => Ok(RequestOutcome::Complete(result)),
            TaskAwareCallToolOutcome::InputRequired(required) => {
                Ok(RequestOutcome::InputRequired(required))
            }
            TaskAwareCallToolOutcome::Task(created) => Err(Error::Transport(format!(
                "tools/call returned task '{}'; use call_tool_once_task_aware for direct task lifecycle control",
                created.task.metadata.task_id
            ))),
        }
    }

    /// Send one `tools/call` attempt and preserve a server-created task.
    ///
    /// Unlike [`call_tool`](Self::call_tool), this does not poll a task or
    /// automatically fulfil input requests. Final-protocol callers can use it
    /// to retain the exact task handle returned from the ordinary request.
    pub async fn call_tool_once_task_aware(
        &self,
        name: &str,
        arguments: serde_json::Value,
        input_responses: Option<InputResponses>,
        request_state: Option<String>,
    ) -> Result<TaskAwareCallToolOutcome> {
        self.ensure_initialized()?;
        let params = CallToolParams {
            name: name.to_string(),
            arguments,
            input_responses,
            request_state,
            meta: None,
            task: None,
        };
        let mut schema_retry_available = self.uses_final_protocol().await;
        self.send_task_aware_tool_request_with_schema_retry(&params, &mut schema_retry_available)
            .await
    }

    /// Request direct control of a tool task lifecycle.
    ///
    /// Instead of blocking until the tool finishes, the server creates a
    /// task and immediately returns a [`CreateTaskResult`] carrying the task
    /// id. Poll with [`task_get`](Self::task_get) or block with
    /// [`task_wait`](Self::task_wait); a completed task's `result` field
    /// carries the [`CallToolResult`] the synchronous call would have
    /// returned.
    ///
    /// On 2025-11-25, `ttl_ms` is sent in the legacy task-augmentation field.
    /// On 2026-07-28, task creation is server-directed: this sends an ordinary
    /// request and requires the server to elect a task. A final client cannot
    /// request a TTL, so a non-`None` `ttl_ms` is rejected on that lifecycle.
    pub async fn call_tool_as_task(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ttl_ms: Option<u64>,
    ) -> Result<CreateTaskResult> {
        self.ensure_initialized()?;
        if self.uses_final_protocol().await {
            if ttl_ms.is_some() {
                return Err(Error::Transport(
                    "ttl_ms is server-selected by the final Tasks extension".to_string(),
                ));
            }
            let params = CallToolParams {
                name: name.to_string(),
                arguments,
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            };
            let mut schema_retry_available = true;
            return match self
                .send_task_aware_tool_request_with_schema_retry(
                    &params,
                    &mut schema_retry_available,
                )
                .await?
            {
                TaskAwareCallToolOutcome::Task(created) => {
                    Ok(Self::legacy_create_task_from_final(created))
                }
                TaskAwareCallToolOutcome::Complete(_) => Err(Error::Transport(
                    "server completed tools/call synchronously; final task creation is server-directed"
                        .to_string(),
                )),
                TaskAwareCallToolOutcome::InputRequired(_) => Err(Error::Transport(
                    "server requested input instead of creating a task".to_string(),
                )),
            };
        }

        let params = CallToolParams {
            name: name.to_string(),
            arguments,
            input_responses: None,
            request_state: None,
            meta: None,
            task: Some(TaskRequestParams { ttl: ttl_ms }),
        };
        let mut schema_retry_available = self.uses_final_protocol().await;
        self.send_tool_request_with_schema_retry(&params, &mut schema_retry_available)
            .await
    }

    /// Fetch a task's current state via `tasks/get` (SEP-2663).
    ///
    /// For `completed` tasks the returned object carries the terminal
    /// [`CallToolResult`] in its `result` field; for `failed` tasks the
    /// JSON-RPC error is in `error`. Unknown or expired task ids surface as
    /// an invalid-params error from the server.
    pub async fn task_get(&self, task_id: &str) -> Result<TaskObject> {
        self.ensure_initialized()?;
        if self.uses_final_protocol().await {
            return Self::legacy_task_from_final(self.task_get_detailed(task_id).await?);
        }
        let params = GetTaskInfoParams {
            task_id: task_id.to_string(),
            meta: None,
        };
        self.send_request("tasks/get", &params).await
    }

    /// Fetch the exact final-protocol `tasks/get` result.
    ///
    /// This preserves status-specific payloads, including all outstanding
    /// `inputRequests`. It is available only after selecting the 2026-07-28
    /// lifecycle; legacy callers should use [`task_get`](Self::task_get).
    pub async fn task_get_detailed(&self, task_id: &str) -> Result<crate::tasks::GetTaskResult> {
        self.ensure_initialized()?;
        if !self.uses_final_protocol().await {
            return Err(Error::Transport(
                "task_get_detailed requires the 2026-07-28 client lifecycle".to_string(),
            ));
        }
        let params = crate::tasks::GetTaskParams {
            task_id: task_id.to_string(),
            meta: None,
        };
        self.send_request("tasks/get", &params).await
    }

    /// Cancel a task via `tasks/cancel` (SEP-2663).
    ///
    /// Cancellation is cooperative: the acknowledgment is an empty result
    /// and the observable status may remain non-terminal for a while after
    /// the ack; poll [`task_get`](Self::task_get) to observe the terminal
    /// state. `reason` is a legacy-only field and is omitted on the final
    /// protocol. The ack body is discarded, so legacy peers that return the
    /// task object are also tolerated.
    pub async fn task_cancel(&self, task_id: &str, reason: Option<String>) -> Result<()> {
        self.ensure_initialized()?;
        if self.uses_final_protocol().await {
            let params = crate::tasks::CancelTaskParams {
                task_id: task_id.to_string(),
                meta: None,
            };
            let _ack: crate::tasks::CancelTaskResult =
                self.send_request("tasks/cancel", &params).await?;
            return Ok(());
        }
        let params = CancelTaskParams {
            task_id: task_id.to_string(),
            reason,
            meta: None,
        };
        let _ack: serde_json::Value = self.send_request("tasks/cancel", &params).await?;
        Ok(())
    }

    /// Answer a task's outstanding input requests via `tasks/update`
    /// (SEP-2663).
    ///
    /// Responses are matched to outstanding requests by key. Final-protocol
    /// callers read the keys from the `inputRequests` of an `input_required`
    /// task returned by [`task_get_detailed`](Self::task_get_detailed).
    ///
    /// A partial map is valid and expected: requests left unanswered stay
    /// outstanding and the task remains `input_required` until every one is
    /// answered. Keys the server does not currently have outstanding, whether
    /// unknown, already answered, or superseded by a later request, are
    /// ignored rather than rejected, so replaying a stale update is safe.
    ///
    /// The acknowledgment carries no data and is discarded. Poll
    /// [`task_get`](Self::task_get) to observe the resulting state.
    pub async fn task_update(&self, task_id: &str, input_responses: InputResponses) -> Result<()> {
        self.ensure_initialized()?;
        if self.uses_final_protocol().await {
            let params = crate::tasks::UpdateTaskParams {
                task_id: task_id.to_string(),
                input_responses,
                meta: None,
            };
            let _ack: crate::tasks::UpdateTaskResult =
                self.send_request("tasks/update", &params).await?;
            return Ok(());
        }
        let input_responses = input_responses
            .into_iter()
            .map(|(key, response)| serde_json::to_value(response).map(|value| (key, value)))
            .collect::<std::result::Result<_, _>>()?;
        let params = UpdateTaskParams {
            task_id: task_id.to_string(),
            input_responses,
            meta: None,
        };
        let _ack: serde_json::Value = self.send_request("tasks/update", &params).await?;
        Ok(())
    }

    /// Poll `tasks/get` until the task reaches a terminal state.
    ///
    /// Honors the server's suggested polling interval (default 1000 ms,
    /// clamped to 50 ms..30 s). On the final protocol it also fulfils
    /// `input_required` requests through the registered client handlers. A
    /// task purged after its TTL surfaces as the server's task-not-found
    /// error. Wrap in
    /// [`tokio::time::timeout`] to bound the overall wait.
    pub async fn task_wait(&self, task_id: &str) -> Result<TaskObject> {
        if self.uses_final_protocol().await {
            let result = self.wait_for_final_task(task_id).await?;
            return Self::legacy_task_from_final(result);
        }
        loop {
            let task = self.task_get(task_id).await?;
            if task.status.is_terminal() {
                return Ok(task);
            }
            let interval_ms = task.poll_interval.unwrap_or(1000).clamp(50, 30_000);
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
        }
    }

    /// List available resources.
    pub async fn list_resources(&self) -> Result<ListResourcesResult> {
        self.list_resources_with_cursor(None).await
    }

    /// Read a resource.
    pub async fn read_resource(&self, uri: &str) -> Result<ReadResourceResult> {
        self.ensure_initialized()?;
        let cache_enabled = self.uses_final_protocol().await && self.response_cache.enabled();
        let generation = if cache_enabled {
            self.response_cache
                .capture_generation("resources/read", uri)
                .await
        } else {
            0
        };
        let mut generation_active = cache_enabled;
        let mut stale = None;
        if cache_enabled {
            match self.response_cache.lookup("resources/read", uri).await {
                CacheLookup::Fresh(value) => {
                    if let Some(result) = decode_cached(&value, "resources/read") {
                        self.response_cache
                            .release_generation("resources/read", uri)
                            .await;
                        return Ok(result);
                    }
                    self.response_cache.evict_resource(uri).await;
                }
                CacheLookup::Stale(value) => stale = Some(value),
                CacheLookup::Miss => {}
            }
        }

        let mut input_responses = None;
        let mut request_state = None;
        let mut followed_input_required = false;
        for round in 0..=self.max_mrtr_rounds {
            let outcome = match self
                .read_resource_once(uri, input_responses.take(), request_state.take())
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    if generation_active {
                        self.response_cache
                            .release_generation("resources/read", uri)
                            .await;
                    }
                    if self.response_cache.serve_stale_on_error()
                        && let Some(value) = stale.as_ref()
                        && let Some(result) = decode_cached(value, "resources/read")
                    {
                        tracing::warn!(
                            uri,
                            error = %error,
                            "Serving stale resources/read response after refresh failure"
                        );
                        return Ok(result);
                    }
                    return Err(error);
                }
            };
            match outcome {
                RequestOutcome::Complete(result) => {
                    if generation_active && !followed_input_required {
                        self.write_cached_response("resources/read", uri, generation, &result)
                            .await;
                    }
                    return Ok(result);
                }
                RequestOutcome::InputRequired(required) => {
                    if !followed_input_required {
                        followed_input_required = true;
                        if generation_active {
                            self.response_cache
                                .release_generation("resources/read", uri)
                                .await;
                            generation_active = false;
                        }
                    }
                    if round == self.max_mrtr_rounds {
                        return Err(Error::Transport(format!(
                            "MRTR round limit ({}) exceeded for resources/read",
                            self.max_mrtr_rounds
                        )));
                    }
                    let requests = required.input_requests.ok_or_else(|| {
                        Error::Transport(
                            "input_required result has no requests the client can fulfil"
                                .to_string(),
                        )
                    })?;
                    input_responses = Some(self.resolve_input_requests(requests).await?);
                    request_state = required.request_state;
                }
            }
        }
        unreachable!("MRTR loop either completes or returns at the configured bound")
    }

    /// Send one resources/read attempt without automatically following MRTR.
    pub async fn read_resource_once(
        &self,
        uri: &str,
        input_responses: Option<InputResponses>,
        request_state: Option<String>,
    ) -> Result<RequestOutcome<ReadResourceResult>> {
        self.ensure_initialized()?;
        let params = ReadResourceParams {
            uri: uri.to_string(),
            input_responses,
            request_state,
            meta: None,
        };
        self.send_request("resources/read", &params).await
    }

    /// Open a final-protocol `subscriptions/listen` notification stream.
    ///
    /// The returned handle owns the long-lived request. Use
    /// [`SubscriptionHandle::acknowledged`] to inspect the subset accepted by
    /// the server, [`SubscriptionHandle::wait`] to observe graceful server
    /// closure, or [`SubscriptionHandle::cancel`] to close the stream.
    /// Notifications continue to flow through the configured
    /// [`ClientHandler`] and carry their subscription ID in
    /// [`ServerNotification::Subscription`].
    pub async fn listen_subscriptions(
        &self,
        notifications: SubscriptionFilter,
    ) -> Result<SubscriptionHandle> {
        self.ensure_initialized()?;
        if !self.uses_final_protocol().await {
            return Err(Error::Transport(
                "subscriptions/listen requires the 2026-07-28 protocol".to_string(),
            ));
        }

        let params = SubscriptionsListenParams {
            notifications: Some(notifications),
            meta: None,
        };
        let params = serde_json::to_value(params).map_err(|error| {
            Error::Transport(format!(
                "failed to serialize subscriptions/listen params: {error}"
            ))
        })?;
        let params = self.with_final_request_meta(params).await?;
        let (id_tx, id_rx) = oneshot::channel();
        let (acknowledgment_tx, acknowledgment_rx) = oneshot::channel();
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(LoopCommand::StartSubscription {
                params,
                id_tx,
                acknowledgment_tx,
                response_tx,
            })
            .await
            .map_err(|_| Error::Transport("connection closed".to_string()))?;
        let request_id = id_rx
            .await
            .map_err(|_| Error::Transport("connection closed".to_string()))?;

        Ok(SubscriptionHandle {
            request_id,
            command_tx: self.command_tx.clone(),
            acknowledgment_rx: Some(acknowledgment_rx),
            response_rx: Some(response_rx),
            active: true,
        })
    }

    /// Subscribe to `notifications/resources/updated` for one resource
    /// (`resources/subscribe`).
    ///
    /// The updates themselves arrive through the notification handler, so a
    /// client that subscribes without registering
    /// [`NotificationHandler::on_resource_updated`] will not see them. Servers
    /// that support this advertise `resources.subscribe` in their
    /// capabilities; one that does not will reject the request.
    pub async fn subscribe_resource(&self, uri: &str) -> Result<()> {
        self.ensure_initialized()?;
        if self.uses_final_protocol().await {
            return Err(Error::Transport(
                "resources/subscribe was removed in 2026-07-28; use subscriptions/listen"
                    .to_string(),
            ));
        }
        let _: serde_json::Value = self
            .send_request("resources/subscribe", &serde_json::json!({ "uri": uri }))
            .await?;
        Ok(())
    }

    /// Stop receiving updates for a resource (`resources/unsubscribe`).
    pub async fn unsubscribe_resource(&self, uri: &str) -> Result<()> {
        self.ensure_initialized()?;
        if self.uses_final_protocol().await {
            return Err(Error::Transport(
                "resources/unsubscribe was removed in 2026-07-28; use subscriptions/listen"
                    .to_string(),
            ));
        }
        let _: serde_json::Value = self
            .send_request("resources/unsubscribe", &serde_json::json!({ "uri": uri }))
            .await?;
        Ok(())
    }

    /// List available prompts.
    pub async fn list_prompts(&self) -> Result<ListPromptsResult> {
        self.list_prompts_with_cursor(None).await
    }

    /// List tools with an optional pagination cursor.
    pub async fn list_tools_with_cursor(&self, cursor: Option<String>) -> Result<ListToolsResult> {
        self.ensure_initialized()?;
        let cache_key = pagination_cache_key(cursor.as_deref());
        self.send_cacheable_request(
            "tools/list",
            &cache_key,
            &ListToolsParams { cursor, meta: None },
        )
        .await
    }

    /// List resources with an optional pagination cursor.
    pub async fn list_resources_with_cursor(
        &self,
        cursor: Option<String>,
    ) -> Result<ListResourcesResult> {
        self.ensure_initialized()?;
        let cache_key = pagination_cache_key(cursor.as_deref());
        self.send_cacheable_request(
            "resources/list",
            &cache_key,
            &ListResourcesParams { cursor, meta: None },
        )
        .await
    }

    /// List resource templates.
    pub async fn list_resource_templates(&self) -> Result<ListResourceTemplatesResult> {
        self.list_resource_templates_with_cursor(None).await
    }

    /// List resource templates with an optional pagination cursor.
    pub async fn list_resource_templates_with_cursor(
        &self,
        cursor: Option<String>,
    ) -> Result<ListResourceTemplatesResult> {
        self.ensure_initialized()?;
        let cache_key = pagination_cache_key(cursor.as_deref());
        self.send_cacheable_request(
            "resources/templates/list",
            &cache_key,
            &ListResourceTemplatesParams { cursor, meta: None },
        )
        .await
    }

    /// List prompts with an optional pagination cursor.
    pub async fn list_prompts_with_cursor(
        &self,
        cursor: Option<String>,
    ) -> Result<ListPromptsResult> {
        self.ensure_initialized()?;
        let cache_key = pagination_cache_key(cursor.as_deref());
        self.send_cacheable_request(
            "prompts/list",
            &cache_key,
            &ListPromptsParams { cursor, meta: None },
        )
        .await
    }

    /// List all tools, following pagination cursors until exhausted.
    pub async fn list_all_tools(&self) -> Result<Vec<ToolDefinition>> {
        let mut all = Vec::new();
        let mut cursor = None;
        loop {
            let result = self.list_tools_with_cursor(cursor).await?;
            all.extend(result.tools);
            match result.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(all)
    }

    /// List all resources, following pagination cursors until exhausted.
    pub async fn list_all_resources(&self) -> Result<Vec<ResourceDefinition>> {
        let mut all = Vec::new();
        let mut cursor = None;
        loop {
            let result = self.list_resources_with_cursor(cursor).await?;
            all.extend(result.resources);
            match result.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(all)
    }

    /// List all resource templates, following pagination cursors until exhausted.
    pub async fn list_all_resource_templates(&self) -> Result<Vec<ResourceTemplateDefinition>> {
        let mut all = Vec::new();
        let mut cursor = None;
        loop {
            let result = self.list_resource_templates_with_cursor(cursor).await?;
            all.extend(result.resource_templates);
            match result.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(all)
    }

    /// List all prompts, following pagination cursors until exhausted.
    pub async fn list_all_prompts(&self) -> Result<Vec<PromptDefinition>> {
        let mut all = Vec::new();
        let mut cursor = None;
        loop {
            let result = self.list_prompts_with_cursor(cursor).await?;
            all.extend(result.prompts);
            match result.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(all)
    }

    /// Call a tool and return the concatenated text content.
    ///
    /// Returns the text from all [`Text`](crate::protocol::Content::Text) items joined together.
    /// If the tool result indicates an error (`is_error` is true), returns
    /// an error with the text content as the message.
    ///
    /// For more control over the result, use [`call_tool()`](Self::call_tool).
    pub async fn call_tool_text(&self, name: &str, arguments: serde_json::Value) -> Result<String> {
        let result = self.call_tool(name, arguments).await?;
        if result.is_error {
            return Err(Error::Internal(result.all_text()));
        }
        Ok(result.all_text())
    }

    /// Get a prompt.
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<GetPromptResult> {
        let arguments = arguments.unwrap_or_default();
        let mut input_responses = None;
        let mut request_state = None;
        for round in 0..=self.max_mrtr_rounds {
            match self
                .get_prompt_once(
                    name,
                    arguments.clone(),
                    input_responses.take(),
                    request_state.take(),
                )
                .await?
            {
                RequestOutcome::Complete(result) => return Ok(result),
                RequestOutcome::InputRequired(required) => {
                    if round == self.max_mrtr_rounds {
                        return Err(Error::Transport(format!(
                            "MRTR round limit ({}) exceeded for prompts/get",
                            self.max_mrtr_rounds
                        )));
                    }
                    let requests = required.input_requests.ok_or_else(|| {
                        Error::Transport(
                            "input_required result has no requests the client can fulfil"
                                .to_string(),
                        )
                    })?;
                    input_responses = Some(self.resolve_input_requests(requests).await?);
                    request_state = required.request_state;
                }
            }
        }
        unreachable!("MRTR loop either completes or returns at the configured bound")
    }

    /// Send one prompts/get attempt without automatically following MRTR.
    pub async fn get_prompt_once(
        &self,
        name: &str,
        arguments: std::collections::HashMap<String, String>,
        input_responses: Option<InputResponses>,
        request_state: Option<String>,
    ) -> Result<RequestOutcome<GetPromptResult>> {
        self.ensure_initialized()?;
        let params = GetPromptParams {
            name: name.to_string(),
            arguments,
            input_responses,
            request_state,
            meta: None,
        };
        self.send_request("prompts/get", &params).await
    }

    /// Ping the server.
    pub async fn ping(&self) -> Result<()> {
        if self.uses_final_protocol().await {
            return Err(Error::Transport(
                "ping was removed from the 2026-07-28 core protocol".to_string(),
            ));
        }
        let _: serde_json::Value = self.send_request("ping", &serde_json::json!({})).await?;
        Ok(())
    }

    /// Request completion suggestions from the server.
    pub async fn complete(
        &self,
        reference: CompletionReference,
        argument_name: &str,
        argument_value: &str,
    ) -> Result<CompleteResult> {
        self.ensure_initialized()?;
        let params = CompleteParams {
            reference,
            argument: CompletionArgument::new(argument_name, argument_value),
            context: None,
            meta: None,
        };
        self.send_request("completion/complete", &params).await
    }

    /// Request completion for a prompt argument.
    pub async fn complete_prompt_arg(
        &self,
        prompt_name: &str,
        argument_name: &str,
        argument_value: &str,
    ) -> Result<CompleteResult> {
        self.complete(
            CompletionReference::prompt(prompt_name),
            argument_name,
            argument_value,
        )
        .await
    }

    /// Request completion for a resource URI.
    pub async fn complete_resource_uri(
        &self,
        resource_uri: &str,
        argument_name: &str,
        argument_value: &str,
    ) -> Result<CompleteResult> {
        self.complete(
            CompletionReference::resource(resource_uri),
            argument_name,
            argument_value,
        )
        .await
    }

    /// Send a raw typed request to the server.
    pub async fn request<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R> {
        self.send_request(method, params).await
    }

    /// Send a raw typed notification to the server.
    pub async fn notify<P: serde::Serialize>(&self, method: &str, params: &P) -> Result<()> {
        self.send_notification(method, params).await
    }

    /// Get the current roots.
    pub async fn roots(&self) -> Vec<Root> {
        self.roots.read().await.clone()
    }

    /// Set roots and notify the server if initialized.
    pub async fn set_roots(&self, roots: Vec<Root>) -> Result<()> {
        *self.roots.write().await = roots;
        if self.is_initialized() && !self.uses_final_protocol().await {
            self.send_notification(notifications::ROOTS_LIST_CHANGED, &serde_json::json!({}))
                .await?;
        }
        Ok(())
    }

    /// Add a root and notify the server if initialized.
    pub async fn add_root(&self, root: Root) -> Result<()> {
        self.roots.write().await.push(root);
        if self.is_initialized() && !self.uses_final_protocol().await {
            self.send_notification(notifications::ROOTS_LIST_CHANGED, &serde_json::json!({}))
                .await?;
        }
        Ok(())
    }

    /// Remove a root by URI and notify the server if initialized.
    pub async fn remove_root(&self, uri: &str) -> Result<bool> {
        let mut roots = self.roots.write().await;
        let initial_len = roots.len();
        roots.retain(|r| r.uri != uri);
        let removed = roots.len() < initial_len;
        drop(roots);

        if removed && self.is_initialized() && !self.uses_final_protocol().await {
            self.send_notification(notifications::ROOTS_LIST_CHANGED, &serde_json::json!({}))
                .await?;
        }
        Ok(removed)
    }

    /// Get the roots list result (for responding to server's roots/list request).
    pub async fn list_roots(&self) -> ListRootsResult {
        ListRootsResult {
            roots: self.roots.read().await.clone(),
            meta: None,
        }
    }

    /// Gracefully shut down the client and close the transport.
    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.command_tx.send(LoopCommand::Shutdown).await;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        Ok(())
    }

    // --- Internal helpers ---

    async fn send_task_aware_tool_request_with_schema_retry(
        &self,
        params: &CallToolParams,
        retry_available: &mut bool,
    ) -> Result<TaskAwareCallToolOutcome> {
        self.send_tool_request_with_schema_retry(params, retry_available)
            .await
    }

    async fn wait_for_final_task(&self, task_id: &str) -> Result<crate::tasks::GetTaskResult> {
        loop {
            let task = self.task_get_detailed(task_id).await?;
            match task.task.status() {
                crate::protocol::TaskStatus::Completed
                | crate::protocol::TaskStatus::Failed
                | crate::protocol::TaskStatus::Cancelled => return Ok(task),
                crate::protocol::TaskStatus::InputRequired => {
                    let requests = task.task.input_requests().cloned().ok_or_else(|| {
                        Error::Transport(format!(
                            "task '{task_id}' is input_required without inputRequests"
                        ))
                    })?;
                    if requests.is_empty() {
                        return Err(Error::Transport(format!(
                            "task '{task_id}' is input_required without inputRequests"
                        )));
                    }
                    let responses = self.resolve_input_requests(requests).await?;
                    self.task_update(task_id, responses).await?;
                }
                crate::protocol::TaskStatus::Working => {
                    let interval_ms = task
                        .task
                        .metadata()
                        .poll_interval_ms
                        .unwrap_or(1000)
                        .clamp(50, 30_000);
                    tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
                }
                _ => {
                    return Err(Error::Transport(format!(
                        "task '{task_id}' returned an unsupported status"
                    )));
                }
            }
        }
    }

    async fn complete_final_task(&self, task_id: &str) -> Result<CallToolResult> {
        let task = self.wait_for_final_task(task_id).await?;
        match task.task.status() {
            crate::protocol::TaskStatus::Completed => {
                let result = task.task.result().cloned().ok_or_else(|| {
                    Error::Transport(format!(
                        "completed task '{task_id}' did not contain a result"
                    ))
                })?;
                serde_json::from_value(serde_json::Value::Object(result)).map_err(|error| {
                    Error::Transport(format!(
                        "failed to deserialize completed task '{task_id}' result: {error}"
                    ))
                })
            }
            crate::protocol::TaskStatus::Failed => {
                let error = task.task.error().cloned().unwrap_or_else(|| {
                    JsonRpcError::internal_error(format!(
                        "task '{task_id}' failed without an error payload"
                    ))
                });
                Err(Error::JsonRpc(error))
            }
            crate::protocol::TaskStatus::Cancelled => {
                Err(Error::Transport(format!("task '{task_id}' was cancelled")))
            }
            _ => Err(Error::Transport(format!(
                "task '{task_id}' did not reach a terminal state"
            ))),
        }
    }

    fn legacy_create_task_from_final(created: crate::tasks::CreateTaskResult) -> CreateTaskResult {
        let metadata = created.task.metadata;
        CreateTaskResult {
            task: TaskObject {
                task_id: metadata.task_id,
                status: created.task.status,
                status_message: metadata.status_message,
                created_at: metadata.created_at,
                last_updated_at: metadata.last_updated_at,
                ttl: metadata.ttl_ms,
                poll_interval: metadata.poll_interval_ms,
                result: None,
                error: None,
                meta: None,
            },
            meta: created.meta.map(serde_json::Value::Object),
        }
    }

    fn legacy_task_from_final(result: crate::tasks::GetTaskResult) -> Result<TaskObject> {
        let task = &result.task;
        let metadata = task.metadata();
        let completed = task
            .result()
            .cloned()
            .map(serde_json::Value::Object)
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| {
                Error::Transport(format!(
                    "failed to deserialize task '{}' result: {error}",
                    metadata.task_id
                ))
            })?;
        Ok(TaskObject {
            task_id: metadata.task_id.clone(),
            status: task.status(),
            status_message: metadata.status_message.clone(),
            created_at: metadata.created_at.clone(),
            last_updated_at: metadata.last_updated_at.clone(),
            ttl: metadata.ttl_ms,
            poll_interval: metadata.poll_interval_ms,
            result: completed,
            error: task.error().cloned(),
            meta: result.meta.map(serde_json::Value::Object),
        })
    }

    async fn send_tool_request_with_schema_retry<R>(
        &self,
        params: &CallToolParams,
        retry_available: &mut bool,
    ) -> Result<R>
    where
        R: serde::de::DeserializeOwned,
    {
        match self.send_request("tools/call", params).await {
            Err(error) if *retry_available && is_stale_tool_schema_error(&error) => {
                *retry_available = false;
                self.response_cache.evict_method("tools/list").await;
                tracing::info!(
                    tool = params.name,
                    error = %error,
                    "Refreshing tools/list before one stale-schema retry"
                );
                if let Err(refresh_error) = self.list_tools().await {
                    tracing::warn!(
                        tool = params.name,
                        error = %refresh_error,
                        "Could not refresh tools/list after stale-schema rejection"
                    );
                    return Err(error);
                }
                self.send_request("tools/call", params).await
            }
            result => result,
        }
    }

    async fn send_cacheable_request<P, R>(
        &self,
        method: &str,
        cache_key: &str,
        params: &P,
    ) -> Result<R>
    where
        P: serde::Serialize,
        R: CacheableResponse,
    {
        let cache_allowed = self.uses_final_protocol().await;
        self.send_cacheable_request_when(method, cache_key, params, cache_allowed)
            .await
    }

    async fn send_cacheable_request_when<P, R>(
        &self,
        method: &str,
        cache_key: &str,
        params: &P,
        cache_allowed: bool,
    ) -> Result<R>
    where
        P: serde::Serialize,
        R: CacheableResponse,
    {
        if !cache_allowed || !self.response_cache.enabled() {
            return self.send_request(method, params).await;
        }

        let generation = self
            .response_cache
            .capture_generation(method, cache_key)
            .await;
        let mut stale = None;
        match self.response_cache.lookup(method, cache_key).await {
            CacheLookup::Fresh(value) => {
                if let Some(result) = decode_cached(&value, method) {
                    self.response_cache
                        .release_generation(method, cache_key)
                        .await;
                    tracing::debug!(method, "Serving fresh response from cache");
                    return Ok(result);
                }
                self.response_cache.evict_method(method).await;
            }
            CacheLookup::Stale(value) => stale = Some(value),
            CacheLookup::Miss => {}
        }

        match self.send_request(method, params).await {
            Ok(result) => {
                self.write_cached_response(method, cache_key, generation, &result)
                    .await;
                Ok(result)
            }
            Err(error) => {
                self.response_cache
                    .release_generation(method, cache_key)
                    .await;
                if self.response_cache.serve_stale_on_error()
                    && let Some(value) = stale.as_ref()
                    && let Some(result) = decode_cached(value, method)
                {
                    tracing::warn!(
                        method,
                        error = %error,
                        "Serving stale response after cache refresh failure"
                    );
                    return Ok(result);
                }
                Err(error)
            }
        }
    }

    async fn write_cached_response<R: CacheableResponse>(
        &self,
        method: &str,
        cache_key: &str,
        generation: u64,
        result: &R,
    ) {
        match serde_json::to_value(result) {
            Ok(value) => {
                self.response_cache
                    .write(
                        method,
                        cache_key,
                        generation,
                        value,
                        result.ttl_ms(),
                        result.cache_scope(),
                    )
                    .await;
            }
            Err(error) => {
                self.response_cache
                    .release_generation(method, cache_key)
                    .await;
                tracing::warn!(
                    method,
                    error = %error,
                    "Skipping response-cache write after serialization failure"
                );
            }
        }
    }

    async fn send_request<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R> {
        let final_protocol = self.uses_final_protocol().await;
        match self.send_request_once(method, params).await {
            Err(Error::SessionExpired)
                if self.supports_session_recovery && !final_protocol && method != "initialize" =>
            {
                tracing::info!(method = %method, "Session expired, attempting recovery");
                self.recover_session().await?;
                self.send_request_once(method, params).await
            }
            other => other,
        }
    }

    async fn send_request_once<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R> {
        self.ensure_connected()?;
        let params_value = serde_json::to_value(params)
            .map_err(|e| Error::Transport(format!("Failed to serialize params: {}", e)))?;
        let params_value = self.with_final_request_meta(params_value).await?;
        let params_value = self.with_progress_token(params_value);

        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(LoopCommand::Request {
                method: method.to_string(),
                params: params_value,
                response_tx,
            })
            .await
            .map_err(|_| Error::Transport("Connection closed".to_string()))?;

        let result = response_rx
            .await
            .map_err(|_| Error::Transport("Connection closed".to_string()))??;

        serde_json::from_value(result)
            .map_err(|e| Error::Transport(format!("Failed to deserialize response: {}", e)))
    }

    /// Recover from a session expiry by resetting the transport and re-initializing.
    async fn recover_session(&self) -> Result<()> {
        // Serialize recovery attempts
        let _guard = self.recovery_lock.lock().await;

        // Check if another task already recovered while we waited
        // (the init_params being present means we were initialized before)
        let init_params = self.init_params.read().await.clone();
        let (client_name, client_version) = match init_params {
            Some(params) => params,
            None => {
                return Err(Error::Transport(
                    "Cannot recover: never initialized".to_string(),
                ));
            }
        };

        // Tell the message loop to reset the transport
        let (done_tx, done_rx) = oneshot::channel();
        self.command_tx
            .send(LoopCommand::ResetSession { done_tx })
            .await
            .map_err(|_| Error::Transport("Connection closed".to_string()))?;
        done_rx
            .await
            .map_err(|_| Error::Transport("Connection closed during recovery".to_string()))?;

        // Clear initialized state
        self.initialized.store(false, Ordering::Release);
        *self.server_info.write().await = None;

        // Re-initialize (using send_request_once to avoid recursion)
        tracing::info!("Re-initializing session after expiry");
        let params = InitializeParams {
            protocol_version: crate::protocol::LATEST_PROTOCOL_VERSION.to_string(),
            capabilities: self.capabilities.clone(),
            client_info: Implementation {
                name: client_name,
                version: client_version,
                ..Default::default()
            },
            meta: None,
        };

        let result: InitializeResult = self.send_request_once("initialize", &params).await?;
        *self.server_info.write().await = Some(result);

        self.send_notification("notifications/initialized", &serde_json::json!({}))
            .await
            .map_err(|error| {
                Error::Transport(format!(
                    "failed to deliver notifications/initialized: {error}"
                ))
            })?;
        self.initialized.store(true, Ordering::Release);

        Ok(())
    }

    async fn send_notification<P: serde::Serialize>(&self, method: &str, params: &P) -> Result<()> {
        self.ensure_connected()?;
        let params_value = serde_json::to_value(params)
            .map_err(|e| Error::Transport(format!("Failed to serialize params: {}", e)))?;
        let params_value = self.with_final_request_meta(params_value).await?;

        // Await the transport result rather than returning on enqueue: a
        // notification the transport failed to deliver must surface here.
        // `initialize()` depends on this for `notifications/initialized`;
        // reporting success while the handshake never completed leaves the
        // session unusable and every later request rejected (#1174).
        let (done_tx, done_rx) = oneshot::channel();
        self.command_tx
            .send(LoopCommand::Notify {
                method: method.to_string(),
                params: params_value,
                done_tx,
            })
            .await
            .map_err(|_| Error::Transport("Connection closed".to_string()))?;

        done_rx
            .await
            .map_err(|_| Error::Transport("Connection closed".to_string()))?
    }

    async fn resolve_input_requests(&self, requests: InputRequests) -> Result<InputResponses> {
        if !self.uses_final_protocol().await {
            return Err(Error::Transport(
                "input_required results require the 2026-07-28 client lifecycle".to_string(),
            ));
        }
        for request in requests.values() {
            let declared = match request {
                InputRequest::CreateMessage(_) => self.capabilities.sampling.is_some(),
                InputRequest::ListRoots(_) => self.capabilities.roots.is_some(),
                InputRequest::Elicit(_) => self.capabilities.elicitation.is_some(),
                _ => false,
            };
            if !declared {
                return Err(Error::Transport(format!(
                    "server requested undeclared MRTR input capability: {}",
                    request.method_name()
                )));
            }
        }

        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(LoopCommand::ResolveInputs {
                requests,
                response_tx,
            })
            .await
            .map_err(|_| Error::Transport("Connection closed".to_string()))?;
        response_rx
            .await
            .map_err(|_| Error::Transport("Connection closed".to_string()))?
    }

    async fn uses_final_protocol(&self) -> bool {
        self.selected_protocol_version.read().await.as_deref()
            == Some(crate::protocol::PROTOCOL_VERSION_2026_07_28)
    }

    fn request_meta_for(&self, version: &str, client_info: &Implementation) -> RequestMeta {
        RequestMeta {
            progress_token: None,
            protocol_version: Some(version.to_string()),
            client_info: Some(client_info.clone()),
            client_capabilities: Some(self.capabilities.clone()),
            log_level: None,
        }
    }

    /// Attach a fresh progress token when the client opted into progress.
    ///
    /// Applied at the one point every typed request method funnels through,
    /// so `call_tool`, `read_resource`, `get_prompt`, and anything added
    /// later all carry a token without each growing an options argument
    /// (#1190). A caller that set its own token keeps it.
    fn with_progress_token(&self, mut params: serde_json::Value) -> serde_json::Value {
        let Some(tokens) = &self.progress_tokens else {
            return params;
        };
        // Only an object can carry `_meta`; a positional or null params value
        // has nowhere to put one.
        let Some(object) = params.as_object_mut() else {
            return params;
        };
        let meta = object
            .entry("_meta")
            .or_insert_with(|| serde_json::json!({}));
        let Some(meta) = meta.as_object_mut() else {
            return params;
        };
        if !meta.contains_key("progressToken") {
            let token = tokens.fetch_add(1, Ordering::Relaxed);
            meta.insert("progressToken".to_string(), serde_json::json!(token));
        }
        params
    }

    async fn with_final_request_meta(
        &self,
        mut params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let Some(version) = self.selected_protocol_version.read().await.clone() else {
            return Ok(params);
        };
        if version != crate::protocol::PROTOCOL_VERSION_2026_07_28 {
            return Ok(params);
        }
        let client_info = self.client_info.read().await.clone().ok_or_else(|| {
            Error::Transport("final protocol selected without client identity".to_string())
        })?;
        let required = serde_json::to_value(self.request_meta_for(&version, &client_info))
            .map_err(|e| Error::Transport(format!("Failed to serialize request metadata: {e}")))?;
        let required = required
            .as_object()
            .expect("RequestMeta serializes as an object");

        if !params.is_object() {
            params = serde_json::json!({});
        }
        let params_object = params
            .as_object_mut()
            .expect("params was normalized to object");
        let meta = params_object
            .entry("_meta")
            .or_insert_with(|| serde_json::json!({}));
        if !meta.is_object() {
            *meta = serde_json::json!({});
        }
        let meta = meta
            .as_object_mut()
            .expect("metadata was normalized to object");
        for (key, value) in required {
            meta.insert(key.clone(), value.clone());
        }

        Ok(params)
    }

    fn ensure_connected(&self) -> Result<()> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(Error::Transport("Connection closed".to_string()));
        }
        Ok(())
    }

    fn ensure_initialized(&self) -> Result<()> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err(Error::Transport("Client not initialized".to_string()));
        }
        Ok(())
    }
}

fn pagination_cache_key(cursor: Option<&str>) -> String {
    serde_json::to_string(&cursor).expect("pagination cursor cache key is serializable")
}

fn is_stale_tool_schema_error(error: &Error) -> bool {
    matches!(
        error,
        Error::JsonRpc(error)
            if error.code == McpErrorCode::HeaderMismatch.code()
                || error.code == ErrorCode::MethodNotFound.code()
                || error.code == ErrorCode::InvalidParams.code()
    )
}

fn decode_cached<R: serde::de::DeserializeOwned>(
    value: &serde_json::Value,
    method: &str,
) -> Option<R> {
    match serde_json::from_value(value.clone()) {
        Ok(result) => Some(result),
        Err(error) => {
            tracing::warn!(
                method,
                error = %error,
                "Discarding response-cache entry that no longer deserializes"
            );
            None
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// =============================================================================
// Background Message Loop
// =============================================================================

/// A pending request waiting for a response from the server.
struct PendingRequest {
    method: String,
    response_tx: oneshot::Sender<Result<serde_json::Value>>,
    acknowledgment_tx: Option<oneshot::Sender<SubscriptionFilter>>,
}

/// Background message loop that multiplexes incoming/outgoing messages.
async fn message_loop<T: ClientTransport, H: ClientHandler>(
    mut transport: T,
    handler: H,
    mut command_rx: mpsc::Receiver<LoopCommand>,
    connected: Arc<AtomicBool>,
    roots: Arc<RwLock<Vec<Root>>>,
    response_cache: Arc<ClientResponseCache>,
) {
    let handler = Arc::new(handler);
    let mut pending_requests: HashMap<RequestId, PendingRequest> = HashMap::new();
    let next_id = AtomicI64::new(1);

    loop {
        tokio::select! {
            // Commands from McpClient methods
            command = command_rx.recv() => {
                match command {
                    Some(LoopCommand::Request { method, params, response_tx }) => {
                        let id = RequestId::Number(next_id.fetch_add(1, Ordering::Relaxed));

                        let request = JsonRpcRequest::new(id.clone(), &method)
                            .with_params(params);
                        let json = match serde_json::to_string(&request) {
                            Ok(j) => j,
                            Err(e) => {
                                let _ = response_tx.send(Err(Error::Transport(
                                    format!("Serialization failed: {}", e)
                                )));
                                continue;
                            }
                        };

                        tracing::debug!(method = %method, id = ?id, "Sending request");
                        pending_requests.insert(id, PendingRequest {
                            method,
                            response_tx,
                            acknowledgment_tx: None,
                        });

                        if let Err(e) = transport.send(&json).await {
                            tracing::error!(error = %e, "Transport send error");
                            fail_all_pending(&mut pending_requests, &format!("Transport error: {}", e));
                            break;
                        }
                    }
                    Some(LoopCommand::StartSubscription {
                        params,
                        id_tx,
                        acknowledgment_tx,
                        response_tx,
                    }) => {
                        let id = RequestId::Number(next_id.fetch_add(1, Ordering::Relaxed));
                        let request = JsonRpcRequest::new(id.clone(), "subscriptions/listen")
                            .with_params(params);
                        let json = match serde_json::to_string(&request) {
                            Ok(json) => json,
                            Err(error) => {
                                let _ = response_tx.send(Err(Error::Transport(
                                    format!("Serialization failed: {error}")
                                )));
                                continue;
                            }
                        };

                        tracing::debug!(id = ?id, "Opening subscription");
                        pending_requests.insert(id.clone(), PendingRequest {
                            method: "subscriptions/listen".to_string(),
                            response_tx,
                            acknowledgment_tx: Some(acknowledgment_tx),
                        });
                        let _ = id_tx.send(id);

                        if let Err(error) = transport.send(&json).await {
                            tracing::error!(%error, "Subscription transport send error");
                            fail_all_pending(
                                &mut pending_requests,
                                &format!("Transport error: {error}"),
                            );
                            break;
                        }
                    }
                    Some(LoopCommand::CancelRequest {
                        request_id,
                        done_tx,
                    }) => {
                        let result = if pending_requests
                            .get(&request_id)
                            .is_some_and(|pending| pending.method == "subscriptions/listen")
                        {
                            let result = transport.cancel_request(&request_id).await;
                            if result.is_ok()
                                && let Some(pending) = pending_requests.remove(&request_id)
                            {
                                let _ = pending.response_tx.send(Err(Error::Transport(
                                    "subscription cancelled".to_string(),
                                )));
                            }
                            result
                        } else {
                            Ok(())
                        };
                        if let Some(done_tx) = done_tx {
                            let _ = done_tx.send(result);
                        }
                    }
                    Some(LoopCommand::Notify { method, params, done_tx }) => {
                        let notification = JsonRpcNotification::new(&method)
                            .with_params(params);
                        let result = match serde_json::to_string(&notification) {
                            Ok(json) => {
                                tracing::debug!(method = %method, "Sending notification");
                                transport.send(&json).await
                            }
                            Err(error) => Err(Error::Transport(format!(
                                "Failed to serialize notification: {error}"
                            ))),
                        };
                        if let Err(error) = &result {
                            tracing::warn!(method = %method, %error, "Notification send failed");
                        }
                        let _ = done_tx.send(result);
                    }
                    Some(LoopCommand::ResolveInputs { requests, response_tx }) => {
                        let result = resolve_inputs_with_handler(&handler, &roots, requests).await;
                        let _ = response_tx.send(result);
                    }
                    Some(LoopCommand::ResetSession { done_tx }) => {
                        tracing::info!("Resetting transport session for re-initialization");
                        transport.reset_session().await;
                        // Fail any pending requests with session expired
                        for (_, pending) in pending_requests.drain() {
                            let _ = pending.response_tx.send(Err(Error::SessionExpired));
                        }
                        let _ = done_tx.send(());
                    }
                    Some(LoopCommand::Shutdown) | None => {
                        tracing::debug!("Message loop shutting down");
                        break;
                    }
                }
            }

            // Incoming messages from the server
            result = transport.recv() => {
                match result {
                    Ok(Some(line)) => {
                        handle_incoming(
                            &line,
                            &mut pending_requests,
                            &handler,
                            &roots,
                            &mut transport,
                            &response_cache,
                        ).await;
                    }
                    Ok(None) => {
                        tracing::info!("Transport closed (EOF)");
                        break;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Transport receive error");
                        break;
                    }
                }
            }
        }
    }

    // Cleanup
    connected.store(false, Ordering::Release);
    fail_all_pending(&mut pending_requests, "Connection closed");
    let _ = transport.close().await;
}

async fn resolve_inputs_with_handler<H: ClientHandler>(
    handler: &Arc<H>,
    roots: &Arc<RwLock<Vec<Root>>>,
    requests: InputRequests,
) -> Result<InputResponses> {
    let mut responses = InputResponses::new();
    for (key, request) in requests {
        let response = match request {
            InputRequest::CreateMessage(params) => InputResponse::CreateMessage(
                handler
                    .handle_create_message(params)
                    .await
                    .map_err(Error::JsonRpc)?,
            ),
            InputRequest::ListRoots(_) => {
                let configured = roots.read().await.clone();
                let result = if configured.is_empty() {
                    handler.handle_list_roots().await.map_err(Error::JsonRpc)?
                } else {
                    ListRootsResult {
                        roots: configured,
                        meta: None,
                    }
                };
                InputResponse::ListRoots(result)
            }
            InputRequest::Elicit(params) => InputResponse::Elicit(
                handler
                    .handle_elicit(params)
                    .await
                    .map_err(Error::JsonRpc)?,
            ),
            _ => {
                return Err(Error::Transport(
                    "unsupported MRTR input request method".to_string(),
                ));
            }
        };
        responses.insert(key, response);
    }
    Ok(responses)
}

/// Handle a single incoming message from the server.
async fn handle_incoming<T: ClientTransport, H: ClientHandler>(
    line: &str,
    pending_requests: &mut HashMap<RequestId, PendingRequest>,
    handler: &Arc<H>,
    roots: &Arc<RwLock<Vec<Root>>>,
    transport: &mut T,
    response_cache: &Arc<ClientResponseCache>,
) {
    let parsed: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to parse incoming message");
            return;
        }
    };

    // Case 1: Response to one of our pending requests (has result or error, no method)
    if parsed.get("method").is_none()
        && (parsed.get("result").is_some() || parsed.get("error").is_some())
    {
        // Check for session-level errors (id: null with -32005) that affect
        // all pending requests, not just a specific one.
        if let Some(error) = parsed.get("error") {
            let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0) as i32;
            let id_missing_or_null = parsed.get("id").is_none_or(|id| id.is_null());
            if code == -32005 && id_missing_or_null {
                tracing::warn!(
                    "Session expired (-32005 with null id), failing all pending requests"
                );
                for (_, pending) in pending_requests.drain() {
                    let _ = pending.response_tx.send(Err(Error::SessionExpired));
                }
                return;
            }
        }

        handle_response(&parsed, pending_requests);
        return;
    }

    // Case 2: Server-initiated request (has id + method)
    if parsed.get("id").is_some() && parsed.get("method").is_some() {
        let id = parse_request_id(&parsed);
        let method = parsed["method"].as_str().unwrap_or("");
        let params = parsed.get("params").cloned();

        let result = dispatch_server_request(handler, roots, method, params).await;

        // Send response back to the server
        let response = match result {
            Ok(value) => {
                if let Some(id) = id {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": value
                    })
                } else {
                    return;
                }
            }
            Err(error) => {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": error.code,
                        "message": error.message
                    }
                })
            }
        };

        if let Ok(json) = serde_json::to_string(&response) {
            let _ = transport.send(&json).await;
        }
        return;
    }

    // Case 3: Server notification (has method, no id)
    if parsed.get("method").is_some() && parsed.get("id").is_none() {
        let method = parsed["method"].as_str().unwrap_or("");
        let params = parsed.get("params").cloned();
        invalidate_response_cache(response_cache, method, params.as_ref()).await;
        let notification = parse_server_notification(method, params);
        let should_dispatch = match &notification {
            ServerNotification::SubscriptionAcknowledged {
                subscription_id,
                notifications,
            } => {
                let Some(key) = matching_subscription_id(pending_requests, subscription_id) else {
                    tracing::warn!(
                        id = ?subscription_id,
                        "Ignoring acknowledgment for unknown subscription"
                    );
                    return;
                };
                if let Some(sender) = pending_requests
                    .get_mut(&key)
                    .and_then(|pending| pending.acknowledgment_tx.take())
                {
                    let _ = sender.send(notifications.clone());
                    true
                } else {
                    tracing::warn!(
                        id = ?subscription_id,
                        "Ignoring duplicate subscription acknowledgment"
                    );
                    false
                }
            }
            ServerNotification::Subscription {
                subscription_id, ..
            } => {
                let Some(key) = matching_subscription_id(pending_requests, subscription_id) else {
                    tracing::warn!(
                        id = ?subscription_id,
                        "Ignoring notification for unknown subscription"
                    );
                    return;
                };
                let before_acknowledgment = pending_requests
                    .get(&key)
                    .is_some_and(|pending| pending.acknowledgment_tx.is_some());
                if before_acknowledgment {
                    tracing::warn!(
                        id = ?subscription_id,
                        "Ending subscription that received a notification before acknowledgment"
                    );
                    if let Some(pending) = pending_requests.remove(&key) {
                        let _ = pending.response_tx.send(Err(Error::Transport(
                            "subscription notification arrived before acknowledgment".to_string(),
                        )));
                    }
                    false
                } else {
                    true
                }
            }
            ServerNotification::SubscriptionCancelled {
                subscription_id,
                reason,
            } => {
                let Some(key) = matching_subscription_id(pending_requests, subscription_id) else {
                    tracing::warn!(
                        id = ?subscription_id,
                        "Ignoring cancellation for unknown or non-subscription request"
                    );
                    return;
                };
                if let Some(pending) = pending_requests.remove(&key) {
                    let message = reason.as_deref().map_or_else(
                        || "subscription cancelled by server".to_string(),
                        |reason| format!("subscription cancelled by server: {reason}"),
                    );
                    let _ = pending.response_tx.send(Err(Error::Transport(message)));
                }
                true
            }
            _ => true,
        };
        if should_dispatch {
            handler.on_notification(notification).await;
        }
    }
}

fn matching_subscription_id(
    pending_requests: &HashMap<RequestId, PendingRequest>,
    id: &RequestId,
) -> Option<RequestId> {
    if pending_requests
        .get(id)
        .is_some_and(|pending| pending.method == "subscriptions/listen")
    {
        return Some(id.clone());
    }
    pending_requests.iter().find_map(|(candidate, pending)| {
        (pending.method == "subscriptions/listen" && request_ids_match(candidate, id))
            .then(|| candidate.clone())
    })
}

fn request_ids_match(left: &RequestId, right: &RequestId) -> bool {
    left == right
        || matches!(
            (left, right),
            (RequestId::Number(number), RequestId::String(value))
                | (RequestId::String(value), RequestId::Number(number))
                if value.parse::<i64>() == Ok(*number)
        )
}

async fn invalidate_response_cache(
    response_cache: &ClientResponseCache,
    method: &str,
    params: Option<&serde_json::Value>,
) {
    match method {
        notifications::TOOLS_LIST_CHANGED => {
            response_cache.evict_method("tools/list").await;
        }
        notifications::PROMPTS_LIST_CHANGED => {
            response_cache.evict_method("prompts/list").await;
        }
        notifications::RESOURCES_LIST_CHANGED => {
            response_cache.evict_method("resources/list").await;
            response_cache
                .evict_method("resources/templates/list")
                .await;
        }
        notifications::RESOURCE_UPDATED => {
            if let Some(uri) = params
                .and_then(|value| value.get("uri"))
                .and_then(serde_json::Value::as_str)
            {
                response_cache.evict_resource(uri).await;
            }
        }
        _ => {}
    }
}

/// Handle a JSON-RPC response by routing to the pending request.
fn handle_response(
    parsed: &serde_json::Value,
    pending_requests: &mut HashMap<RequestId, PendingRequest>,
) {
    let id = match parse_request_id(parsed) {
        Some(id) => id,
        None => {
            tracing::warn!("Response without id");
            return;
        }
    };

    // Exact match first; genuine string IDs always take precedence. As a
    // fallback, accept a response whose id is the string form of a numeric
    // request id ("42" matching 42) -- some servers stringify numeric ids
    // when echoing them (rmcp #1021 analog).
    let pending = match pending_requests.remove(&id) {
        Some(p) => p,
        None => {
            let numeric_fallback = match &id {
                RequestId::String(s) => s
                    .parse::<i64>()
                    .ok()
                    .and_then(|n| pending_requests.remove(&RequestId::Number(n))),
                _ => None,
            };
            match numeric_fallback {
                Some(p) => p,
                None => {
                    tracing::warn!(id = ?id, "Response for unknown request");
                    return;
                }
            }
        }
    };

    tracing::debug!(id = ?id, "Received response");

    if let Some(error) = parsed.get("error") {
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-1) as i32;
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown error")
            .to_string();
        let data = error.get("data").cloned();

        // -32005 = SessionNotFound: signal session expiry for recovery
        if code == -32005 {
            let _ = pending.response_tx.send(Err(Error::SessionExpired));
            return;
        }

        let json_rpc_error = JsonRpcError {
            code,
            message,
            data,
        };
        let _ = pending
            .response_tx
            .send(Err(Error::JsonRpc(json_rpc_error)));
    } else if let Some(result) = parsed.get("result") {
        if pending.method == "subscriptions/listen" && pending.acknowledgment_tx.is_some() {
            let _ = pending.response_tx.send(Err(Error::Transport(
                "subscriptions/listen completed before acknowledgment".to_string(),
            )));
            return;
        }
        let _ = pending.response_tx.send(Ok(result.clone()));
    } else {
        let _ = pending
            .response_tx
            .send(Err(Error::Transport("Invalid response".to_string())));
    }
}

/// Dispatch a server-initiated request to the handler.
async fn dispatch_server_request<H: ClientHandler>(
    handler: &Arc<H>,
    roots: &Arc<RwLock<Vec<Root>>>,
    method: &str,
    params: Option<serde_json::Value>,
) -> std::result::Result<serde_json::Value, JsonRpcError> {
    match method {
        "sampling/createMessage" => {
            let p = serde_json::from_value(params.unwrap_or_default())
                .map_err(|e| JsonRpcError::invalid_params(e.to_string()))?;
            let result = handler.handle_create_message(p).await?;
            serde_json::to_value(result).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        }
        "elicitation/create" => {
            let p = serde_json::from_value(params.unwrap_or_default())
                .map_err(|e| JsonRpcError::invalid_params(e.to_string()))?;
            let result = handler.handle_elicit(p).await?;
            serde_json::to_value(result).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        }
        "roots/list" => {
            // Use client-configured roots if available, otherwise delegate to handler
            let roots_list = roots.read().await;
            if !roots_list.is_empty() {
                let result = ListRootsResult {
                    roots: roots_list.clone(),
                    meta: None,
                };
                return serde_json::to_value(result)
                    .map_err(|e| JsonRpcError::internal_error(e.to_string()));
            }
            drop(roots_list);

            let result = handler.handle_list_roots().await?;
            serde_json::to_value(result).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        }
        "ping" => Ok(serde_json::json!({})),
        _ => Err(JsonRpcError::method_not_found(method)),
    }
}

/// Parse a request ID from a JSON-RPC message.
fn parse_request_id(parsed: &serde_json::Value) -> Option<RequestId> {
    parsed.get("id").and_then(|id| {
        if let Some(n) = id.as_i64() {
            Some(RequestId::Number(n))
        } else {
            id.as_str().map(|s| RequestId::String(s.to_string()))
        }
    })
}

/// Parse a server notification into the typed enum.
fn parse_server_notification(
    method: &str,
    params: Option<serde_json::Value>,
) -> ServerNotification {
    if method == notifications::SUBSCRIPTIONS_ACKNOWLEDGED {
        if let Some(params) = &params
            && let Ok(acknowledgment) =
                serde_json::from_value::<SubscriptionsAcknowledgedParams>(params.clone())
            && let Some(subscription_id) = acknowledgment.meta.and_then(|meta| meta.subscription_id)
        {
            return ServerNotification::SubscriptionAcknowledged {
                subscription_id,
                notifications: acknowledgment.notifications,
            };
        }
        return ServerNotification::Unknown {
            method: method.to_string(),
            params,
        };
    }
    if method == notifications::CANCELLED {
        if let Some(params) = &params
            && let Ok(cancelled) = serde_json::from_value::<CancelledParams>(params.clone())
            && let Some(subscription_id) = cancelled.request_id
        {
            return ServerNotification::SubscriptionCancelled {
                subscription_id,
                reason: cancelled.reason,
            };
        }
        return ServerNotification::Unknown {
            method: method.to_string(),
            params,
        };
    }

    let subscription_id = params
        .as_ref()
        .and_then(|params| params.pointer("/_meta/io.modelcontextprotocol~1subscriptionId"))
        .and_then(|id| serde_json::from_value::<RequestId>(id.clone()).ok());
    let notification = match method {
        notifications::PROGRESS => {
            if let Some(params) = params.clone()
                && let Ok(p) = serde_json::from_value(params)
            {
                ServerNotification::Progress(p)
            } else {
                ServerNotification::Unknown {
                    method: method.to_string(),
                    params: None,
                }
            }
        }
        notifications::MESSAGE => {
            if let Some(params) = params.clone()
                && let Ok(p) = serde_json::from_value(params)
            {
                ServerNotification::LogMessage(p)
            } else {
                ServerNotification::Unknown {
                    method: method.to_string(),
                    params: None,
                }
            }
        }
        notifications::RESOURCE_UPDATED => {
            if let Some(params) = &params
                && let Some(uri) = params.get("uri").and_then(|u| u.as_str())
            {
                ServerNotification::ResourceUpdated {
                    uri: uri.to_string(),
                }
            } else {
                ServerNotification::Unknown {
                    method: method.to_string(),
                    params: params.clone(),
                }
            }
        }
        notifications::RESOURCES_LIST_CHANGED => ServerNotification::ResourcesListChanged,
        notifications::TOOLS_LIST_CHANGED => ServerNotification::ToolsListChanged,
        notifications::PROMPTS_LIST_CHANGED => ServerNotification::PromptsListChanged,
        notifications::TASK_STATUS_CHANGED => {
            let is_final = params
                .as_ref()
                .and_then(serde_json::Value::as_object)
                .is_some_and(|params| params.contains_key("ttlMs"));
            match (is_final, params.clone()) {
                (true, Some(params)) => {
                    match serde_json::from_value::<crate::tasks::TaskStatusNotificationParams>(
                        params.clone(),
                    ) {
                        Ok(params) => ServerNotification::FinalTaskStatusChanged(params),
                        Err(_) => ServerNotification::Unknown {
                            method: method.to_string(),
                            params: Some(params),
                        },
                    }
                }
                (false, Some(params)) => {
                    match serde_json::from_value::<TaskStatusParams>(params.clone()) {
                        Ok(params) => ServerNotification::TaskStatusChanged(params),
                        Err(_) => ServerNotification::Unknown {
                            method: method.to_string(),
                            params: Some(params),
                        },
                    }
                }
                (_, None) => ServerNotification::Unknown {
                    method: method.to_string(),
                    params: None,
                },
            }
        }
        _ => ServerNotification::Unknown {
            method: method.to_string(),
            params: params.clone(),
        },
    };
    if let Some(subscription_id) = subscription_id {
        ServerNotification::Subscription {
            subscription_id,
            notification: Box::new(notification),
        }
    } else {
        notification
    }
}

/// Fail all pending requests with the given error message.
fn fail_all_pending(pending: &mut HashMap<RequestId, PendingRequest>, reason: &str) {
    for (_, req) in pending.drain() {
        let _ = req
            .response_tx
            .send(Err(Error::Transport(reason.to_string())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Mock transport for testing that auto-responds to requests.
    ///
    /// When the client sends a request via `send()`, the mock extracts the
    /// request ID, pairs it with the next preconfigured response, and feeds
    /// it back through a channel that `recv()` awaits on. This ensures
    /// `recv()` blocks when no messages are available (instead of returning
    /// EOF), keeping the background message loop alive.
    struct MockTransport {
        /// Pre-configured result or error replies (not full envelopes).
        responses: Arc<Mutex<Vec<MockReply>>>,
        /// Index of the next response to use.
        response_idx: Arc<std::sync::atomic::AtomicUsize>,
        /// Channel sender for feeding responses back to `recv()`.
        incoming_tx: mpsc::Sender<String>,
        /// Channel receiver for `recv()` to await on.
        incoming_rx: mpsc::Receiver<String>,
        /// Collected outgoing messages from `send()`.
        outgoing: Arc<Mutex<Vec<String>>>,
        connected: Arc<AtomicBool>,
        /// When set, `send()` fails for notifications (messages without an
        /// `id`), simulating a transport that could not deliver them.
        fail_notification_sends: Arc<AtomicBool>,
    }

    enum MockReply {
        Result(serde_json::Value),
        Error(JsonRpcError),
    }

    #[allow(dead_code)]
    impl MockTransport {
        fn new() -> Self {
            let (tx, rx) = mpsc::channel(32);
            Self {
                responses: Arc::new(Mutex::new(Vec::new())),
                response_idx: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                incoming_tx: tx,
                incoming_rx: rx,
                outgoing: Arc::new(Mutex::new(Vec::new())),
                connected: Arc::new(AtomicBool::new(true)),
                fail_notification_sends: Arc::new(AtomicBool::new(false)),
            }
        }

        /// Create a mock that auto-responds with the given result payloads.
        ///
        /// When `send()` receives a JSON-RPC request, it extracts the request
        /// ID and pairs it with the next response from this list, sending the
        /// complete JSON-RPC response through the channel for `recv()`.
        fn with_responses(responses: Vec<serde_json::Value>) -> Self {
            let (tx, rx) = mpsc::channel(32);
            Self {
                responses: Arc::new(Mutex::new(
                    responses.into_iter().map(MockReply::Result).collect(),
                )),
                response_idx: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                incoming_tx: tx,
                incoming_rx: rx,
                outgoing: Arc::new(Mutex::new(Vec::new())),
                connected: Arc::new(AtomicBool::new(true)),
                fail_notification_sends: Arc::new(AtomicBool::new(false)),
            }
        }

        fn with_replies(responses: Vec<MockReply>) -> Self {
            let (tx, rx) = mpsc::channel(32);
            Self {
                responses: Arc::new(Mutex::new(responses)),
                response_idx: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                incoming_tx: tx,
                incoming_rx: rx,
                outgoing: Arc::new(Mutex::new(Vec::new())),
                connected: Arc::new(AtomicBool::new(true)),
                fail_notification_sends: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    #[async_trait]
    impl ClientTransport for MockTransport {
        async fn send(&mut self, message: &str) -> Result<()> {
            self.outgoing.lock().unwrap().push(message.to_string());

            // Parse the outgoing message to extract the request ID
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(message) {
                if parsed.get("id").is_none()
                    && self.fail_notification_sends.load(Ordering::Relaxed)
                {
                    return Err(Error::Transport(
                        "mock transport dropped the notification".to_string(),
                    ));
                }
                // Only respond to requests (messages with an id and method)
                if let Some(id) = parsed.get("id") {
                    let idx = self
                        .response_idx
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let responses = self.responses.lock().unwrap();
                    if let Some(reply) = responses.get(idx) {
                        let response = match reply {
                            MockReply::Result(result) => serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": result
                            }),
                            MockReply::Error(error) => serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": error
                            }),
                        };
                        let _ = self.incoming_tx.try_send(response.to_string());
                    }
                }
            }

            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<String>> {
            // Await on the channel -- blocks until a message is available
            // or the sender is dropped (returns None = EOF).
            match self.incoming_rx.recv().await {
                Some(msg) => Ok(Some(msg)),
                None => Ok(None),
            }
        }

        fn is_connected(&self) -> bool {
            self.connected.load(Ordering::Relaxed)
        }

        async fn close(&mut self) -> Result<()> {
            self.connected.store(false, Ordering::Relaxed);
            Ok(())
        }
    }

    fn mock_initialize_response() -> serde_json::Value {
        serde_json::json!({
            "protocolVersion": "2025-11-25",
            "serverInfo": {
                "name": "test-server",
                "version": "1.0.0"
            },
            "capabilities": {
                "tools": {}
            }
        })
    }

    /// #1174: a notification the transport fails to deliver must fail
    /// `initialize()`. Reporting success left the handshake incomplete and a
    /// strict server rejecting every subsequent request with -32600.
    #[tokio::test]
    async fn initialize_fails_when_initialized_notification_is_not_delivered() {
        let transport = MockTransport::with_responses(vec![mock_initialize_response()]);
        let fail_notifications = transport.fail_notification_sends.clone();
        let outgoing = transport.outgoing.clone();
        // The initialize request itself succeeds; only the follow-up
        // notification is dropped.
        fail_notifications.store(true, Ordering::Relaxed);

        let client = McpClient::connect(transport).await.unwrap();
        let error = client
            .initialize("test-client", "1.0.0")
            .await
            .expect_err("undelivered notifications/initialized must fail initialize");

        assert!(
            error
                .to_string()
                .contains("failed to deliver notifications/initialized"),
            "error should name the handshake step, got: {error}"
        );
        // The notification was attempted, not skipped.
        assert!(
            outgoing
                .lock()
                .unwrap()
                .iter()
                .any(|message| message.contains("notifications/initialized")),
            "the client must have tried to send the notification"
        );
        // The client must not consider itself initialized.
        assert!(!client.is_initialized());
    }

    #[tokio::test]
    async fn notification_delivery_errors_reach_the_caller() {
        let transport = MockTransport::with_responses(vec![mock_initialize_response()]);
        let fail_notifications = transport.fail_notification_sends.clone();

        let client = McpClient::connect(transport).await.unwrap();
        client.initialize("test-client", "1.0.0").await.unwrap();

        // Healthy so far; now the transport starts dropping notifications.
        fail_notifications.store(true, Ordering::Relaxed);
        let error = client
            .notify("notifications/progress", &serde_json::json!({}))
            .await
            .expect_err("a dropped notification must surface as an error");
        assert!(error.to_string().contains("dropped the notification"));
    }

    #[tokio::test]
    async fn test_client_not_initialized() {
        let client = McpClient::connect(MockTransport::with_responses(vec![]))
            .await
            .unwrap();

        let result = client.list_tools().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }

    #[tokio::test]
    async fn test_client_initialize() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
        ]))
        .await
        .unwrap();

        assert!(!client.is_initialized());

        let result = client.initialize("test-client", "1.0.0").await;
        assert!(result.is_ok());
        assert!(client.is_initialized());

        let server_info = client.server_info().await.unwrap();
        assert_eq!(server_info.server_info.name, "test-server");
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    #[tokio::test]
    async fn final_discover_injects_metadata_on_every_request() {
        let transport = MockTransport::with_responses(vec![
            serde_json::json!({
                "resultType": "complete",
                "supportedVersions": ["2026-07-28"],
                "capabilities": {}
            }),
            serde_json::json!({
                "resultType": "complete",
                "tools": [],
                "ttlMs": 0,
                "cacheScope": "private"
            }),
        ]);
        let outgoing = transport.outgoing.clone();
        let client = McpClient::builder()
            .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
            .with_elicitation()
            .connect_simple(transport)
            .await
            .unwrap();

        client.discover("test-client", "1.0.0").await.unwrap();
        client.list_tools().await.unwrap();
        assert_eq!(
            client.selected_protocol_version().await.as_deref(),
            Some("2026-07-28")
        );

        let messages: Vec<serde_json::Value> = outgoing
            .lock()
            .unwrap()
            .iter()
            .map(|message| serde_json::from_str(message).unwrap())
            .collect();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["method"], "server/discover");
        assert_eq!(messages[1]["method"], "tools/list");
        for message in messages {
            let meta = &message["params"]["_meta"];
            assert_eq!(
                meta["io.modelcontextprotocol/protocolVersion"],
                "2026-07-28"
            );
            assert_eq!(
                meta["io.modelcontextprotocol/clientInfo"]["name"],
                "test-client"
            );
            assert!(meta["io.modelcontextprotocol/clientCapabilities"].is_object());
        }
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    fn final_discover_result() -> serde_json::Value {
        serde_json::json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {},
            "ttlMs": 0,
            "cacheScope": "private"
        })
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    struct SubscriptionTestTransport {
        incoming_tx: mpsc::Sender<String>,
        incoming_rx: mpsc::Receiver<String>,
        outgoing: Arc<Mutex<Vec<String>>>,
        connected: Arc<AtomicBool>,
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    impl SubscriptionTestTransport {
        fn new() -> Self {
            let (incoming_tx, incoming_rx) = mpsc::channel(32);
            Self {
                incoming_tx,
                incoming_rx,
                outgoing: Arc::new(Mutex::new(Vec::new())),
                connected: Arc::new(AtomicBool::new(true)),
            }
        }
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    #[async_trait]
    impl ClientTransport for SubscriptionTestTransport {
        async fn send(&mut self, message: &str) -> Result<()> {
            self.outgoing.lock().unwrap().push(message.to_string());
            let value: serde_json::Value = serde_json::from_str(message)
                .map_err(|error| Error::Transport(error.to_string()))?;
            let Some(id) = value.get("id").cloned() else {
                return Ok(());
            };
            match value
                .get("method")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
            {
                "server/discover" => {
                    self.incoming_tx
                        .send(
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": final_discover_result()
                            })
                            .to_string(),
                        )
                        .await
                        .map_err(|_| Error::Transport("test channel closed".to_string()))?;
                }
                "subscriptions/listen" => {
                    let notifications = value["params"]["notifications"].clone();
                    self.incoming_tx
                        .send(
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": notifications::SUBSCRIPTIONS_ACKNOWLEDGED,
                                "params": {
                                    "_meta": {
                                        "io.modelcontextprotocol/subscriptionId": id
                                    },
                                    "notifications": notifications
                                }
                            })
                            .to_string(),
                        )
                        .await
                        .map_err(|_| Error::Transport("test channel closed".to_string()))?;

                    if value["params"]["notifications"]["promptsListChanged"]
                        == serde_json::Value::Bool(true)
                    {
                        self.incoming_tx
                            .send(
                                serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "resultType": "complete",
                                        "_meta": {
                                            "io.modelcontextprotocol/subscriptionId": id
                                        }
                                    }
                                })
                                .to_string(),
                            )
                            .await
                            .map_err(|_| Error::Transport("test channel closed".to_string()))?;
                    } else {
                        self.incoming_tx
                            .send(
                                serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "method": notifications::TOOLS_LIST_CHANGED,
                                    "params": {
                                        "_meta": {
                                            "io.modelcontextprotocol/subscriptionId": id
                                        }
                                    }
                                })
                                .to_string(),
                            )
                            .await
                            .map_err(|_| Error::Transport("test channel closed".to_string()))?;
                    }
                }
                _ => {}
            }
            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<String>> {
            Ok(self.incoming_rx.recv().await)
        }

        fn is_connected(&self) -> bool {
            self.connected.load(Ordering::Acquire)
        }

        async fn close(&mut self) -> Result<()> {
            self.connected.store(false, Ordering::Release);
            Ok(())
        }
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    #[tokio::test]
    async fn final_subscriptions_correlate_and_cancel_over_message_transport() {
        let transport = SubscriptionTestTransport::new();
        let outgoing = transport.outgoing.clone();
        let incoming = transport.incoming_tx.clone();
        let received = Arc::new(Mutex::new(Vec::new()));

        struct RecordingHandler(Arc<Mutex<Vec<ServerNotification>>>);

        #[async_trait]
        impl ClientHandler for RecordingHandler {
            async fn on_notification(&self, notification: ServerNotification) {
                self.0.lock().unwrap().push(notification);
            }
        }

        let client = McpClient::builder()
            .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
            .connect(transport, RecordingHandler(received.clone()))
            .await
            .unwrap();
        client.discover("test-client", "1.0.0").await.unwrap();

        let requested = SubscriptionFilter {
            tools_list_changed: Some(true),
            ..Default::default()
        };
        let mut first = client
            .listen_subscriptions(requested.clone())
            .await
            .unwrap();
        let mut second = client.listen_subscriptions(requested).await.unwrap();
        assert_ne!(first.id(), second.id());
        assert_eq!(
            first.acknowledged().await.unwrap().tools_list_changed,
            Some(true)
        );
        assert_eq!(
            second.acknowledged().await.unwrap().tools_list_changed,
            Some(true)
        );

        for _ in 0..100 {
            if received
                .lock()
                .unwrap()
                .iter()
                .filter(|notification| {
                    matches!(
                        notification,
                        ServerNotification::Subscription {
                            notification,
                            ..
                        } if matches!(notification.as_ref(), ServerNotification::ToolsListChanged)
                    )
                })
                .count()
                == 2
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        let subscription_ids: Vec<RequestId> = received
            .lock()
            .unwrap()
            .iter()
            .filter_map(|notification| match notification {
                ServerNotification::Subscription {
                    subscription_id,
                    notification,
                } if matches!(notification.as_ref(), ServerNotification::ToolsListChanged) => {
                    Some(subscription_id.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(subscription_ids, [first.id().clone(), second.id().clone()]);

        incoming
            .send(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": notifications::CANCELLED,
                    "params": {
                        "requestId": 999,
                        "reason": "not a subscription"
                    }
                })
                .to_string(),
            )
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert!(
            !received.lock().unwrap().iter().any(|notification| matches!(
                notification,
                ServerNotification::SubscriptionCancelled { subscription_id, .. }
                    if subscription_id == &RequestId::Number(999)
            ))
        );

        let first_id = first.id().clone();
        let second_id = second.id().clone();
        first.cancel().await.unwrap();
        second.cancel().await.unwrap();
        let cancellation_ids: Vec<RequestId> = outgoing
            .lock()
            .unwrap()
            .iter()
            .filter_map(|message| {
                let value: serde_json::Value = serde_json::from_str(message).unwrap();
                (value["method"] == notifications::CANCELLED)
                    .then(|| serde_json::from_value(value["params"]["requestId"].clone()).unwrap())
            })
            .collect();
        assert_eq!(cancellation_ids, [first_id, second_id]);
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    #[tokio::test]
    async fn final_subscription_observes_graceful_completion() {
        let client = McpClient::builder()
            .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
            .connect_simple(SubscriptionTestTransport::new())
            .await
            .unwrap();
        client.discover("test-client", "1.0.0").await.unwrap();

        let mut handle = client
            .listen_subscriptions(SubscriptionFilter {
                prompts_list_changed: Some(true),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            handle.acknowledged().await.unwrap().prompts_list_changed,
            Some(true)
        );
        let expected_id = handle.id().clone();
        let result = handle.wait().await.unwrap();
        assert!(result.result_type.is_complete());
        assert_eq!(result.meta.subscription_id, expected_id);
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    #[tokio::test]
    async fn final_subscription_accepts_server_cancellation_only_for_active_listen() {
        let transport = SubscriptionTestTransport::new();
        let incoming = transport.incoming_tx.clone();
        let client = McpClient::builder()
            .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
            .connect_simple(transport)
            .await
            .unwrap();
        client.discover("test-client", "1.0.0").await.unwrap();

        let mut handle = client
            .listen_subscriptions(SubscriptionFilter {
                tools_list_changed: Some(true),
                ..Default::default()
            })
            .await
            .unwrap();
        handle.acknowledged().await.unwrap();
        let id = handle.id().clone();
        incoming
            .send(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": notifications::CANCELLED,
                    "params": {
                        "requestId": id,
                        "reason": "server shutdown"
                    }
                })
                .to_string(),
            )
            .await
            .unwrap();

        let error = handle.wait().await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("subscription cancelled by server: server shutdown")
        );
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    fn cacheable_tools_result(name: &str) -> serde_json::Value {
        serde_json::json!({
            "resultType": "complete",
            "tools": [{
                "name": name,
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            }],
            "ttlMs": 60_000,
            "cacheScope": "private"
        })
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    #[tokio::test]
    async fn final_cache_serves_a_fresh_list_without_a_round_trip() {
        let transport = MockTransport::with_responses(vec![
            final_discover_result(),
            cacheable_tools_result("cached"),
        ]);
        let outgoing = transport.outgoing.clone();
        let client = McpClient::builder()
            .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
            .connect_simple(transport)
            .await
            .unwrap();
        client.discover("test-client", "1.0.0").await.unwrap();

        let first = client.list_tools().await.unwrap();
        let second = client.list_tools().await.unwrap();

        assert_eq!(first.tools[0].name, "cached");
        assert_eq!(second.tools[0].name, "cached");
        assert_eq!(outgoing.lock().unwrap().len(), 2);
        assert_eq!(client.response_cache_len().await, 1);
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    #[tokio::test]
    async fn disabling_final_cache_forces_each_list_request() {
        let transport = MockTransport::with_responses(vec![
            final_discover_result(),
            cacheable_tools_result("first"),
            cacheable_tools_result("second"),
        ]);
        let outgoing = transport.outgoing.clone();
        let client = McpClient::builder()
            .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
            .disable_response_cache()
            .connect_simple(transport)
            .await
            .unwrap();
        client.discover("test-client", "1.0.0").await.unwrap();

        assert_eq!(client.list_tools().await.unwrap().tools[0].name, "first");
        assert_eq!(client.list_tools().await.unwrap().tools[0].name, "second");
        assert_eq!(outgoing.lock().unwrap().len(), 3);
        assert_eq!(client.response_cache_len().await, 0);
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    #[tokio::test]
    async fn list_changed_notification_invalidates_a_fresh_entry() {
        let transport = MockTransport::with_responses(vec![
            final_discover_result(),
            cacheable_tools_result("first"),
            cacheable_tools_result("second"),
        ]);
        let incoming = transport.incoming_tx.clone();
        let outgoing = transport.outgoing.clone();
        let notification_seen = Arc::new(AtomicBool::new(false));
        let handler = NotificationHandler::new().on_tools_changed({
            let notification_seen = notification_seen.clone();
            move || notification_seen.store(true, Ordering::Release)
        });
        let client = McpClient::builder()
            .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
            .connect(transport, handler)
            .await
            .unwrap();
        client.discover("test-client", "1.0.0").await.unwrap();
        assert_eq!(client.list_tools().await.unwrap().tools[0].name, "first");

        incoming
            .send(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": notifications::TOOLS_LIST_CHANGED,
                    "params": {}
                })
                .to_string(),
            )
            .await
            .unwrap();
        for _ in 0..100 {
            if notification_seen.load(Ordering::Acquire) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(notification_seen.load(Ordering::Acquire));

        assert_eq!(client.list_tools().await.unwrap().tools[0].name, "second");
        assert_eq!(outgoing.lock().unwrap().len(), 3);
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    #[tokio::test]
    async fn rotating_private_partition_refetches_resource() {
        let resource_result = |text: &str| {
            serde_json::json!({
                "resultType": "complete",
                "contents": [{
                    "uri": "config://app",
                    "text": text
                }],
                "ttlMs": 60_000,
                "cacheScope": "private"
            })
        };
        let transport = MockTransport::with_responses(vec![
            final_discover_result(),
            resource_result("principal-a"),
            resource_result("principal-b"),
        ]);
        let outgoing = transport.outgoing.clone();
        let client = McpClient::builder()
            .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
            .response_cache(ClientCacheConfig::default().with_partition("principal-a"))
            .connect_simple(transport)
            .await
            .unwrap();
        client.discover("test-client", "1.0.0").await.unwrap();

        assert_eq!(
            client
                .read_resource("config://app")
                .await
                .unwrap()
                .first_text(),
            Some("principal-a")
        );
        client.set_cache_partition("principal-b").await;
        assert_eq!(
            client
                .read_resource("config://app")
                .await
                .unwrap()
                .first_text(),
            Some("principal-b")
        );
        assert_eq!(outgoing.lock().unwrap().len(), 3);
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    #[tokio::test]
    async fn final_tool_call_refreshes_stale_schema_and_retries_once() {
        let transport = MockTransport::with_replies(vec![
            MockReply::Result(final_discover_result()),
            MockReply::Result(cacheable_tools_result("changing-tool")),
            MockReply::Error(JsonRpcError::header_mismatch("stale x-mcp-header mapping")),
            MockReply::Result(cacheable_tools_result("changing-tool")),
            MockReply::Result(serde_json::json!({
                "resultType": "complete",
                "content": [{"type": "text", "text": "retried"}]
            })),
        ]);
        let outgoing = transport.outgoing.clone();
        let client = McpClient::builder()
            .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
            .connect_simple(transport)
            .await
            .unwrap();
        client.discover("test-client", "1.0.0").await.unwrap();
        client.list_tools().await.unwrap();

        let result = client
            .call_tool("changing-tool", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result.first_text(), Some("retried"));

        let methods: Vec<String> = outgoing
            .lock()
            .unwrap()
            .iter()
            .map(|message| {
                serde_json::from_str::<serde_json::Value>(message).unwrap()["method"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            methods,
            [
                "server/discover",
                "tools/list",
                "tools/call",
                "tools/list",
                "tools/call"
            ]
        );
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    #[tokio::test]
    async fn final_call_tool_as_task_sends_no_legacy_task_parameter() {
        let transport = MockTransport::with_responses(vec![
            final_discover_result(),
            serde_json::json!({
                "resultType": "task",
                "taskId": "task-final",
                "status": "working",
                "createdAt": "2026-07-31T00:00:00Z",
                "lastUpdatedAt": "2026-07-31T00:00:00Z",
                "ttlMs": null,
                "pollIntervalMs": 50
            }),
        ]);
        let outgoing = transport.outgoing.clone();
        let client = McpClient::builder()
            .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
            .with_tasks()
            .connect_simple(transport)
            .await
            .unwrap();
        client.discover("test-client", "1.0.0").await.unwrap();

        let created = client
            .call_tool_as_task("long-tool", serde_json::json!({}), None)
            .await
            .unwrap();
        assert_eq!(created.task.task_id, "task-final");

        let messages = outgoing.lock().unwrap();
        let call: serde_json::Value = serde_json::from_str(&messages[1]).unwrap();
        assert_eq!(call["method"], "tools/call");
        assert!(
            call["params"].get("task").is_none(),
            "final tools/call leaked the legacy task parameter: {call}"
        );
        assert!(
            call["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"]["extensions"]
                .get(crate::protocol::TASKS_EXTENSION_ID)
                .is_some()
        );
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    #[tokio::test]
    async fn final_stale_schema_retry_is_bounded() {
        let transport = MockTransport::with_replies(vec![
            MockReply::Result(final_discover_result()),
            MockReply::Result(cacheable_tools_result("changing-tool")),
            MockReply::Error(JsonRpcError::header_mismatch("first rejection")),
            MockReply::Result(cacheable_tools_result("changing-tool")),
            MockReply::Error(JsonRpcError::header_mismatch("second rejection")),
        ]);
        let outgoing = transport.outgoing.clone();
        let client = McpClient::builder()
            .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
            .connect_simple(transport)
            .await
            .unwrap();
        client.discover("test-client", "1.0.0").await.unwrap();
        client.list_tools().await.unwrap();

        let error = client
            .call_tool("changing-tool", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Error::JsonRpc(error) if error.code == McpErrorCode::HeaderMismatch.code()
        ));
        assert_eq!(outgoing.lock().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn legacy_tool_call_does_not_retry_a_header_mismatch() {
        let transport = MockTransport::with_replies(vec![
            MockReply::Result(mock_initialize_response()),
            MockReply::Error(JsonRpcError::header_mismatch("legacy rejection")),
        ]);
        let outgoing = transport.outgoing.clone();
        let client = McpClient::connect(transport).await.unwrap();
        client.initialize("test-client", "1.0.0").await.unwrap();

        let error = client
            .call_tool("changing-tool", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Error::JsonRpc(error) if error.code == McpErrorCode::HeaderMismatch.code()
        ));
        let messages = outgoing.lock().unwrap();
        assert_eq!(messages.len(), 3);
        assert!(
            !messages
                .iter()
                .any(|message| message.contains("tools/list"))
        );
    }

    #[test]
    fn stale_tool_schema_errors_are_pre_execution_protocol_errors() {
        for error in [
            Error::JsonRpc(JsonRpcError::header_mismatch("mismatch")),
            Error::JsonRpc(JsonRpcError::method_not_found("tool")),
            Error::JsonRpc(JsonRpcError::invalid_params("arguments")),
        ] {
            assert!(is_stale_tool_schema_error(&error));
        }
        assert!(!is_stale_tool_schema_error(&Error::JsonRpc(
            JsonRpcError::internal_error("executed")
        )));
    }

    #[tokio::test]
    async fn test_list_tools() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "tools": [
                    {
                        "name": "test_tool",
                        "description": "A test tool",
                        "inputSchema": {
                            "type": "object",
                            "properties": {}
                        }
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let tools = client.list_tools().await.unwrap();

        assert_eq!(tools.tools.len(), 1);
        assert_eq!(tools.tools[0].name, "test_tool");
    }

    #[tokio::test]
    async fn test_call_tool() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "content": [
                    {
                        "type": "text",
                        "text": "Tool result"
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let result = client
            .call_tool("test_tool", serde_json::json!({"arg": "value"}))
            .await
            .unwrap();

        assert!(!result.content.is_empty());
    }

    #[tokio::test]
    async fn test_list_resources() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "resources": [
                    {
                        "uri": "file://test.txt",
                        "name": "Test File"
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let resources = client.list_resources().await.unwrap();

        assert_eq!(resources.resources.len(), 1);
        assert_eq!(resources.resources[0].uri, "file://test.txt");
    }

    #[tokio::test]
    async fn test_read_resource() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "contents": [
                    {
                        "uri": "file://test.txt",
                        "text": "File contents"
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let result = client.read_resource("file://test.txt").await.unwrap();

        assert_eq!(result.contents.len(), 1);
        assert_eq!(result.contents[0].text.as_deref(), Some("File contents"));
    }

    #[tokio::test]
    async fn test_list_prompts() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "prompts": [
                    {
                        "name": "test_prompt",
                        "description": "A test prompt"
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let prompts = client.list_prompts().await.unwrap();

        assert_eq!(prompts.prompts.len(), 1);
        assert_eq!(prompts.prompts[0].name, "test_prompt");
    }

    #[tokio::test]
    async fn test_get_prompt() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "messages": [
                    {
                        "role": "user",
                        "content": {
                            "type": "text",
                            "text": "Prompt message"
                        }
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let result = client.get_prompt("test_prompt", None).await.unwrap();

        assert_eq!(result.messages.len(), 1);
    }

    #[tokio::test]
    async fn test_ping() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({}),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let result = client.ping().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_with_roots() {
        let roots = vec![Root::new("file:///test")];
        let client = McpClient::builder()
            .with_roots(roots)
            .connect_simple(MockTransport::with_responses(vec![]))
            .await
            .unwrap();

        let current_roots = client.roots().await;
        assert_eq!(current_roots.len(), 1);
    }

    #[tokio::test]
    async fn test_roots_management() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
        ]))
        .await
        .unwrap();

        // Initially no roots
        assert!(client.roots().await.is_empty());

        // Add a root before initialization (no notification sent)
        client.add_root(Root::new("file:///project")).await.unwrap();
        assert_eq!(client.roots().await.len(), 1);

        // Initialize
        client.initialize("test-client", "1.0.0").await.unwrap();

        // Remove a root
        let removed = client.remove_root("file:///project").await.unwrap();
        assert!(removed);
        assert!(client.roots().await.is_empty());

        // Try to remove non-existent root
        let not_removed = client.remove_root("file:///nonexistent").await.unwrap();
        assert!(!not_removed);
    }

    #[tokio::test]
    async fn test_list_roots() {
        let roots = vec![
            Root::new("file:///project1"),
            Root::with_name("file:///project2", "Project 2"),
        ];
        let client = McpClient::builder()
            .with_roots(roots)
            .connect_simple(MockTransport::with_responses(vec![]))
            .await
            .unwrap();

        let result = client.list_roots().await;
        assert_eq!(result.roots.len(), 2);
        assert_eq!(result.roots[1].name, Some("Project 2".to_string()));
    }

    #[test]
    fn test_builder_with_sampling() {
        let builder = McpClientBuilder::new().with_sampling();
        assert!(builder.capabilities.sampling.is_some());
    }

    #[test]
    fn test_builder_with_elicitation() {
        let builder = McpClientBuilder::new().with_elicitation();
        assert!(builder.capabilities.elicitation.is_some());
    }

    #[test]
    fn builder_adds_protocol_extension_without_replacing_other_capabilities() {
        let extension = crate::ExtensionDeclaration::new(
            "com.example/rendering",
            serde_json::json!({"formats": ["html"]}),
        )
        .unwrap();
        let builder = McpClientBuilder::new()
            .with_sampling()
            .with_protocol_extension(extension);

        assert!(builder.capabilities.sampling.is_some());
        assert_eq!(
            builder.capabilities.extensions.as_ref().unwrap()["com.example/rendering"]["formats"]
                [0],
            "html"
        );
    }

    #[test]
    fn test_builder_chaining() {
        let builder = McpClientBuilder::new()
            .with_sampling()
            .with_elicitation()
            .with_roots(vec![Root::new("file:///project")]);
        assert!(builder.capabilities.sampling.is_some());
        assert!(builder.capabilities.elicitation.is_some());
        assert!(builder.capabilities.roots.is_some());
    }

    #[tokio::test]
    async fn test_bidirectional_sampling_round_trip() {
        use crate::protocol::{
            ContentRole, CreateMessageParams, CreateMessageResult, SamplingContent,
            SamplingContentOrArray,
        };

        // A handler that records whether handle_create_message was called
        struct RecordingHandler {
            called: Arc<AtomicBool>,
        }

        #[async_trait]
        impl ClientHandler for RecordingHandler {
            async fn handle_create_message(
                &self,
                _params: CreateMessageParams,
            ) -> std::result::Result<CreateMessageResult, tower_mcp_types::JsonRpcError>
            {
                self.called.store(true, Ordering::SeqCst);
                Ok(CreateMessageResult {
                    content: SamplingContentOrArray::Single(SamplingContent::Text {
                        text: "test response".to_string(),
                        annotations: None,
                        meta: None,
                    }),
                    model: "test-model".to_string(),
                    role: ContentRole::Assistant,
                    stop_reason: Some("end_turn".to_string()),
                    meta: None,
                })
            }
        }

        let called = Arc::new(AtomicBool::new(false));
        let handler = RecordingHandler {
            called: called.clone(),
        };

        // Build a mock transport, keeping a clone of incoming_tx so we can
        // inject a server-initiated request after the transport is consumed.
        let (inject_tx, rx) = mpsc::channel::<String>(32);
        let responses = vec![mock_initialize_response()];
        let inject_tx_clone = inject_tx.clone();

        let transport = MockTransport {
            responses: Arc::new(Mutex::new(
                responses.into_iter().map(MockReply::Result).collect(),
            )),
            response_idx: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            incoming_tx: inject_tx,
            incoming_rx: rx,
            outgoing: Arc::new(Mutex::new(Vec::new())),
            connected: Arc::new(AtomicBool::new(true)),
            fail_notification_sends: Arc::new(AtomicBool::new(false)),
        };

        let client = McpClient::builder()
            .with_sampling()
            .connect(transport, handler)
            .await
            .unwrap();

        // Initialize the client (this sends initialize request + notification)
        client.initialize("test-client", "1.0.0").await.unwrap();

        // Inject a server-initiated sampling/createMessage request
        let sampling_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "sampling/createMessage",
            "params": {
                "messages": [
                    {
                        "role": "user",
                        "content": {
                            "type": "text",
                            "text": "Hello"
                        }
                    }
                ],
                "maxTokens": 100
            }
        });
        inject_tx_clone
            .send(sampling_request.to_string())
            .await
            .unwrap();

        // Give the background loop time to process
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify the handler was called
        assert!(
            called.load(Ordering::SeqCst),
            "handle_create_message should have been called"
        );
    }

    #[tokio::test]
    async fn test_list_resource_templates() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "resourceTemplates": [
                    {
                        "uriTemplate": "file:///{path}",
                        "name": "File Template",
                        "description": "A file template"
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let result = client.list_resource_templates().await.unwrap();

        assert_eq!(result.resource_templates.len(), 1);
        assert_eq!(result.resource_templates[0].name, "File Template");
    }

    #[tokio::test]
    async fn test_list_all_tools_single_page() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "tools": [
                    {
                        "name": "tool_a",
                        "description": "Tool A",
                        "inputSchema": { "type": "object", "properties": {} }
                    },
                    {
                        "name": "tool_b",
                        "description": "Tool B",
                        "inputSchema": { "type": "object", "properties": {} }
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let tools = client.list_all_tools().await.unwrap();

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "tool_a");
        assert_eq!(tools[1].name, "tool_b");
    }

    #[tokio::test]
    async fn test_list_all_tools_paginated() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            // First page with a next_cursor
            serde_json::json!({
                "tools": [
                    {
                        "name": "tool_a",
                        "description": "Tool A",
                        "inputSchema": { "type": "object", "properties": {} }
                    }
                ],
                "nextCursor": "page2"
            }),
            // Second page with no next_cursor
            serde_json::json!({
                "tools": [
                    {
                        "name": "tool_b",
                        "description": "Tool B",
                        "inputSchema": { "type": "object", "properties": {} }
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let tools = client.list_all_tools().await.unwrap();

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "tool_a");
        assert_eq!(tools[1].name, "tool_b");
    }

    #[tokio::test]
    async fn test_call_tool_text_success() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "content": [
                    { "type": "text", "text": "Hello " },
                    { "type": "text", "text": "World" }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let text = client
            .call_tool_text("test_tool", serde_json::json!({}))
            .await
            .unwrap();

        assert_eq!(text, "Hello World");
    }

    #[tokio::test]
    async fn test_call_tool_text_error() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "content": [
                    { "type": "text", "text": "something went wrong" }
                ],
                "isError": true
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let result = client
            .call_tool_text("test_tool", serde_json::json!({}))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("something went wrong"),
            "Error message should contain tool error text, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_server_notification_parsing() {
        let notification = parse_server_notification("notifications/tools/list_changed", None);
        assert!(matches!(notification, ServerNotification::ToolsListChanged));

        let notification = parse_server_notification("notifications/resources/list_changed", None);
        assert!(matches!(
            notification,
            ServerNotification::ResourcesListChanged
        ));

        let notification = parse_server_notification(
            "notifications/resources/updated",
            Some(serde_json::json!({"uri": "file:///test"})),
        );
        match notification {
            ServerNotification::ResourceUpdated { uri } => {
                assert_eq!(uri, "file:///test");
            }
            _ => panic!("Expected ResourceUpdated"),
        }

        let notification =
            parse_server_notification("custom/notification", Some(serde_json::json!({"data": 42})));
        match notification {
            ServerNotification::Unknown { method, params } => {
                assert_eq!(method, "custom/notification");
                assert!(params.is_some());
            }
            _ => panic!("Expected Unknown"),
        }

        let notification = parse_server_notification(
            notifications::SUBSCRIPTIONS_ACKNOWLEDGED,
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/subscriptionId": 7
                },
                "notifications": {
                    "toolsListChanged": true
                }
            })),
        );
        assert!(matches!(
            notification,
            ServerNotification::SubscriptionAcknowledged {
                subscription_id: RequestId::Number(7),
                ..
            }
        ));

        let notification = parse_server_notification(
            notifications::TOOLS_LIST_CHANGED,
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/subscriptionId": "stream-a"
                }
            })),
        );
        assert!(matches!(
            notification,
            ServerNotification::Subscription {
                subscription_id: RequestId::String(id),
                notification,
            } if id == "stream-a"
                && matches!(notification.as_ref(), ServerNotification::ToolsListChanged)
        ));

        let notification = parse_server_notification(
            notifications::CANCELLED,
            Some(serde_json::json!({
                "requestId": 7,
                "reason": "done"
            })),
        );
        assert!(matches!(
            notification,
            ServerNotification::SubscriptionCancelled {
                subscription_id: RequestId::Number(7),
                reason: Some(reason),
            } if reason == "done"
        ));

        let notification = parse_server_notification(
            notifications::TASK_STATUS_CHANGED,
            Some(serde_json::json!({
                "taskId": "legacy-task",
                "status": "completed",
                "createdAt": "2026-08-02T00:00:00Z",
                "lastUpdatedAt": "2026-08-02T00:00:01Z",
                "ttl": null
            })),
        );
        assert!(matches!(
            notification,
            ServerNotification::TaskStatusChanged(TaskStatusParams {
                task_id,
                status: crate::protocol::TaskStatus::Completed,
                ..
            }) if task_id == "legacy-task"
        ));

        let notification = parse_server_notification(
            notifications::TASK_STATUS_CHANGED,
            Some(serde_json::json!({
                "taskId": "final-task",
                "status": "cancelled",
                "createdAt": "2026-08-02T00:00:00Z",
                "lastUpdatedAt": "2026-08-02T00:00:01Z",
                "ttlMs": null,
                "_meta": {
                    "io.modelcontextprotocol/subscriptionId": "task-stream"
                }
            })),
        );
        assert!(matches!(
            notification,
            ServerNotification::Subscription {
                subscription_id: RequestId::String(id),
                notification,
            } if id == "task-stream"
                && matches!(
                    notification.as_ref(),
                    ServerNotification::FinalTaskStatusChanged(params)
                        if params.task.task_id() == "final-task"
                            && params.task.status() == crate::protocol::TaskStatus::Cancelled
                )
        ));
    }

    // =========================================================================
    // handle_response ID correlation
    // =========================================================================

    fn pending_with(
        ids: &[RequestId],
    ) -> (
        HashMap<RequestId, PendingRequest>,
        Vec<oneshot::Receiver<Result<serde_json::Value>>>,
    ) {
        let mut map = HashMap::new();
        let mut rxs = Vec::new();
        for id in ids {
            let (tx, rx) = oneshot::channel();
            map.insert(
                id.clone(),
                PendingRequest {
                    method: "test".to_string(),
                    response_tx: tx,
                    acknowledgment_tx: None,
                },
            );
            rxs.push(rx);
        }
        (map, rxs)
    }

    #[tokio::test]
    async fn test_stringified_numeric_response_id_correlates() {
        // rmcp #1021 analog: a numeric request id 42 answered with a
        // stringified id "42" still correlates.
        let (mut pending, mut rxs) = pending_with(&[RequestId::Number(42)]);

        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "42",
            "result": {"ok": true}
        });
        handle_response(&response, &mut pending);

        assert!(pending.is_empty(), "pending request should be resolved");
        let result = rxs.remove(0).await.unwrap().unwrap();
        assert_eq!(result, serde_json::json!({"ok": true}));
    }

    #[tokio::test]
    async fn test_exact_string_id_takes_precedence() {
        // A genuine string id "42" must match exactly and win over the
        // numeric interpretation when both are pending.
        let (mut pending, mut rxs) =
            pending_with(&[RequestId::String("42".to_string()), RequestId::Number(42)]);

        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "42",
            "result": {"which": "string"}
        });
        handle_response(&response, &mut pending);

        // The string entry resolved; the numeric entry is still pending.
        assert_eq!(pending.len(), 1);
        assert!(pending.contains_key(&RequestId::Number(42)));
        let result = rxs.remove(0).await.unwrap().unwrap();
        assert_eq!(result, serde_json::json!({"which": "string"}));
    }

    #[tokio::test]
    async fn test_non_numeric_string_id_does_not_correlate() {
        // A string id that is not the string form of the pending numeric
        // id must not resolve it.
        let (mut pending, _rxs) = pending_with(&[RequestId::Number(42)]);

        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "not-a-number",
            "result": {}
        });
        handle_response(&response, &mut pending);

        assert_eq!(pending.len(), 1, "numeric request should stay pending");
    }
}
