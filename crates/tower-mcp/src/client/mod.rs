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
//! # Opening a connection
//!
//! Connecting and starting a session are separate steps, because which session
//! to start is a choice:
//!
//! 1. [`McpClient::connect`] (or [`McpClientBuilder::connect`]) takes the
//!    transport and spawns the message loop. No MCP traffic has happened yet.
//! 2. [`McpClient::initialize`] performs the 2025-11-25 handshake, or
//!    [`McpClient::discover`] starts the sessionless 2026-07-28 lifecycle.
//!    Every other request is refused until one of them has run.
//!
//! Which requests exist depends on that choice, and the difference is not
//! cosmetic: 2026-07-28 removed `ping`, replaced `resources/subscribe` with
//! [`McpClient::listen_subscriptions`], and made task creation the server's
//! decision. The methods that changed say so, and
//! [`crate::guides::protocol_versions`] covers the split.
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
mod oauth_address;
#[cfg(feature = "oauth-client")]
mod oauth_authcode;
#[cfg(feature = "oauth-client")]
mod oauth_flow;
mod response_cache;
mod stdio;
mod transport;
#[cfg(feature = "websocket")]
mod websocket;

pub use channel::ChannelTransport;
pub use handler::{ClientHandler, NotificationHandler, ServerNotification};
#[cfg(feature = "http-client")]
pub use http::{HttpClientConfig, HttpClientTransport, HttpStatusDetails};
#[cfg(feature = "oauth-client")]
pub use oauth::{
    OAuthBearerChallenge, OAuthClientCredentials, OAuthClientCredentialsBuilder, OAuthClientError,
    OAuthRefreshScopePolicy, OAuthScopeChallenge, OAuthScopeEscalationConfig,
    OAuthScopeEscalationHandler, OAuthScopeEscalationRequest, OAuthTokenEndpointAuthMethod,
    TokenProvider,
};
#[cfg(feature = "oauth-client")]
pub use oauth_address::{OAuthAddressPolicy, OAuthAddressResolver};
#[cfg(feature = "oauth-client")]
pub use oauth_authcode::{
    MemoryOAuthClientRegistrationStore, OAuthApplicationType, OAuthAuthCodeConfig,
    OAuthAuthorizationCode, OAuthAuthorizationDiscovery, OAuthAuthorizationServerMetadata,
    OAuthClientRegistration, OAuthClientRegistrationMethod, OAuthClientRegistrationOptions,
    OAuthClientRegistrationStore, OAuthDynamicClientRegistration, OAuthProtectedResourceMetadata,
    discover_oauth_authorization, discover_oauth_authorization_server,
    discover_oauth_authorization_with_policy, probe_oauth_bearer_challenge,
    resolve_oauth_client_registration, resolve_oauth_client_registration_with_store,
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
pub use response_cache::{AggregateList, ClientCacheConfig, DEFAULT_MAX_CACHE_TTL};
pub use stdio::StdioClientTransport;
pub use transport::ClientTransport;
#[cfg(feature = "websocket")]
pub use websocket::{WebSocketClientConfig, WebSocketClientTransport};

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::ProtocolSupport;
use crate::context::CancellationToken;
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
    LogLevel, PromptDefinition, ReadResourceParams, ReadResourceResult, RequestId, RequestMeta,
    RequestOutcome, ResourceDefinition, ResourceTemplateDefinition, Root, RootsCapability,
    SamplingCapability, SubscriptionFilter, SubscriptionsAcknowledgedParams,
    SubscriptionsListenParams, SubscriptionsListenResult, TaskObject, TaskRequestParams,
    TaskStatusParams, ToolDefinition, UpdateTaskParams, notifications,
};
use response_cache::{CacheLookup, ClientResponseCache};
use serde::Deserialize;
use tower_mcp_types::JsonRpcError;

/// Decides whether a client may dispatch a prepared tool call.
///
/// The client awaits this hook on the calling task. Implementations can await
/// approval and may be called concurrently. See
/// [`McpClientBuilder::tool_call_policy`] for coverage and decision lifetime.
#[async_trait::async_trait]
pub trait ToolCallPolicy: Send + Sync {
    /// Decide whether to send this tool-call attempt.
    async fn authorize(&self, call: ToolCallContext<'_>) -> ToolCallDecision;
}

#[async_trait::async_trait]
impl<T: ToolCallPolicy + ?Sized> ToolCallPolicy for Arc<T> {
    async fn authorize(&self, call: ToolCallContext<'_>) -> ToolCallDecision {
        (**self).authorize(call).await
    }
}

/// The prepared parameters and connection context for one tool-call attempt.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct ToolCallContext<'a> {
    /// The MCP tool name as it appears in the request.
    pub name: &'a str,
    /// The arguments as serialized, including an explicit JSON null if present.
    pub arguments: Option<&'a serde_json::Value>,
    /// The complete serialized parameters, including request metadata and any
    /// continuation fields. The client sends these parameters unchanged on allow.
    pub params: &'a serde_json::Value,
    /// Whether the request carries legacy task augmentation. Final-protocol
    /// task creation is server-directed, so an ordinary call can return a task.
    /// An explicit `"task": null` leaves this flag false and stays in `params`.
    pub task_augmented: bool,
    /// The negotiated MCP version, if a lifecycle handshake has completed.
    pub protocol_version: Option<&'a str>,
    /// The client-local generation, advanced by legacy session resets.
    pub connection_generation: u64,
}

impl<'a> ToolCallContext<'a> {
    /// Construct a context from prepared parameters, for example to unit-test
    /// a policy directly. Pass the raw tool name from those parameters.
    pub fn new(
        name: &'a str,
        params: &'a serde_json::Value,
        protocol_version: Option<&'a str>,
        connection_generation: u64,
    ) -> Self {
        Self {
            name,
            arguments: params.get("arguments"),
            params,
            task_augmented: params["task"].is_object(),
            protocol_version,
            connection_generation,
        }
    }
}

/// The policy's decision for one prepared tool-call attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolCallDecision {
    /// Allow the client to dispatch the request.
    Allow,
    /// Reject the request locally.
    Deny {
        /// A caller-supplied reason suitable for returning in an error.
        reason: String,
    },
}

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

/// Cancels an in-flight request if its caller's future is dropped.
///
/// Disarmed as soon as the response arrives, so only an abandoned request is
/// cancelled. Uses `try_send` because `Drop` cannot await: if the command
/// channel is full or closed the cancellation is skipped rather than blocking
/// a drop, which is the same trade-off `SubscriptionHandle` already makes.
struct CancelOnDrop {
    id: RequestId,
    command_tx: mpsc::Sender<LoopCommand>,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.command_tx.try_send(LoopCommand::CancelRequest {
                request_id: self.id.clone(),
                done_tx: None,
            });
        }
    }
}

/// Internal command sent from McpClient methods to the background loop.
enum LoopCommand {
    /// Send a JSON-RPC request and await a response.
    ///
    /// The id is allocated by the caller rather than the loop, so a caller
    /// can cancel the request it sent (#1202). The loop previously owned the
    /// counter, which left callers with no way to name their own request.
    Request {
        generation: u64,
        id: RequestId,
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
    /// Cancel one in-flight request, of any method.
    CancelRequest {
        request_id: RequestId,
        done_tx: Option<oneshot::Sender<Result<()>>>,
    },
    /// Send a JSON-RPC notification (no JSON-RPC response expected; the
    /// completion channel reports whether the transport delivered it).
    Notify {
        generation: u64,
        method: String,
        params: serde_json::Value,
        done_tx: oneshot::Sender<Result<()>>,
    },
    /// Reset the transport's session state for re-initialization.
    ResetSession {
        /// Terminate a session left by a failed initialize on the server.
        /// An already-expired session only needs local cleanup.
        end_on_server: bool,
        done_tx: oneshot::Sender<()>,
    },
    /// Fulfil embedded MRTR requests through the configured client handler.
    ResolveInputs {
        generation: u64,
        requests: InputRequests,
        response_tx: oneshot::Sender<Result<InputResponses>>,
    },
    /// Graceful shutdown.
    Shutdown,
}

/// An active `subscriptions/listen` request.
///
/// Returned by [`McpClient::listen_subscriptions`], which is refused unless
/// the 2026-07-28 lifecycle is active: this is what replaced
/// `resources/subscribe` there.
///
/// The handle exposes the JSON-RPC request ID used as the subscription ID,
/// the server's acknowledged filter, graceful server completion, and explicit
/// cancellation. Dropping an active handle requests cancellation on a
/// best-effort basis; callers that need confirmation should call
/// [`cancel()`](Self::cancel).
///
/// The notifications themselves do not arrive through this handle. They reach
/// the client's [`NotificationHandler`], tagged with the subscription ID, so a
/// client that opens a stream without registering one receives nothing it can
/// observe.
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
/// tasks. Responses are correlated by JSON-RPC id, so two tasks sharing one
/// client do not queue behind each other and a slow tool call does not hold up
/// a `tools/list` issued after it.
///
/// # Ending the connection
///
/// [`shutdown()`](Self::shutdown) asks the loop to stop and waits for it.
/// Dropping the client instead aborts the loop where it stands, so anything
/// still in flight is abandoned and the transport closes without a parting
/// message. Prefer `shutdown` wherever the server cares about a clean close.
///
/// # Example
///
/// [`ChannelTransport`] connects to a router in the same process, which is
/// what makes this runnable here. Swap it for [`StdioClientTransport`] or
/// `HttpClientTransport` and nothing else changes:
///
/// ```rust
/// use tower_mcp::client::{ChannelTransport, McpClient};
/// use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};
/// use schemars::JsonSchema;
/// use serde::Deserialize;
///
/// #[derive(Debug, Deserialize, JsonSchema)]
/// struct AddInput {
///     a: i64,
///     b: i64,
/// }
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), tower_mcp::BoxError> {
/// let add = ToolBuilder::new("add")
///     .description("Add two numbers")
///     .handler(|input: AddInput| async move {
///         Ok(CallToolResult::text((input.a + input.b).to_string()))
///     })
///     .build();
/// let server = McpRouter::new().server_info("calculator", "1.0.0").tool(add);
///
/// let client = McpClient::connect(ChannelTransport::new(server)).await?;
///
/// // Nothing else is permitted until the session is open.
/// let server_info = client.initialize("my-client", "1.0.0").await?;
/// assert_eq!(server_info.server_info.name, "calculator");
///
/// let tools = client.list_tools().await?;
/// assert_eq!(tools.tools[0].name, "add");
///
/// let result = client.call_tool("add", serde_json::json!({"a": 2, "b": 3})).await?;
/// assert_eq!(result.all_text(), "5");
///
/// client.shutdown().await?;
/// # Ok(())
/// # }
/// ```
pub struct McpClient {
    session: Arc<ClientSession>,
    /// Background task handle.
    task: Option<JoinHandle<()>>,
    /// Server discovery result, when the final lifecycle is active.
    discovery: RwLock<Option<DiscoverResult>>,
    /// Current roots (shared with the loop for roots/list responses).
    roots: Arc<RwLock<Vec<Root>>>,
    /// Whether the transport supports session recovery.
    supports_session_recovery: bool,
    /// Maximum number of input-required rounds auto-driven per operation.
    max_mrtr_rounds: usize,
    /// Maximum number of pages one `list_all_*` call fetches.
    max_list_pages: usize,
    /// SEP-2549 final-protocol response cache.
    response_cache: Arc<ClientResponseCache>,
}

/// Client-side state shared by request callers and the passive HTTP session recovery path.
struct ClientSession {
    /// Channel to send commands to the background loop.
    command_tx: mpsc::Sender<LoopCommand>,
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
    /// Whether the transport is still connected.
    connected: AtomicBool,
    /// Client identity from the last successful legacy handshake, for recovery.
    recovery_client_info: RwLock<Option<Implementation>>,
    /// Lock to prevent concurrent session recovery attempts.
    recovery_lock: Mutex<()>,
    /// Allocator for per-request progress tokens; `None` when the client did
    /// not opt into progress.
    progress_tokens: Option<Arc<AtomicI64>>,
    /// Default log threshold attached to future 2026-07-28 requests.
    request_log_level: RwLock<Option<LogLevel>>,
    /// Source of the trace context attached to each request's `_meta`.
    trace_context_provider: Option<TraceContextProvider>,
    /// Allocator for JSON-RPC request ids, shared with the message loop so
    /// caller-issued and loop-issued ids never collide.
    next_request_id: AtomicI64,
    /// Incremented by the transport-owning loop whenever it resets a session.
    generation: AtomicU64,
    /// Set as soon as the message loop learns the current session has expired.
    expired: AtomicBool,
    tool_call_policy: Option<Box<dyn ToolCallPolicy>>,
}

/// Default page budget for one automatic list walk.
///
/// A `list_all_*` call that fetches this many pages and is handed another
/// `nextCursor` fails with [`Error::ListPageLimitExceeded`]. Change it with
/// [`McpClientBuilder::max_list_pages`].
///
/// One thousand pages is far beyond any catalog a server is expected to
/// serve: even at ten items per page it covers ten thousand tools, and
/// servers built on this crate return every item in one page unless
/// configured otherwise. It still stops a server that hands out cursors
/// forever after a bounded number of round trips rather than never.
pub const DEFAULT_MAX_LIST_PAGES: usize = 1000;

/// One page of a list walk, reduced to the parts pagination needs.
struct ListPage<T> {
    items: Vec<T>,
    next_cursor: Option<String>,
    ttl_ms: Option<u64>,
    cache_scope: Option<CacheScope>,
}

/// Source of the W3C trace context attached to outgoing requests.
type TraceContextProvider =
    Arc<dyn Fn() -> Option<crate::trace_context::TraceContext> + Send + Sync>;

/// Settings a builder hands to the connect path, grouped so the private
/// constructor keeps one parameter per concern rather than one per field.
struct ClientSettings {
    capabilities: ClientCapabilities,
    roots: Vec<Root>,
    protocol_support: ProtocolSupport,
    max_mrtr_rounds: usize,
    max_list_pages: usize,
    cache_config: ClientCacheConfig,
    request_progress: bool,
    request_log_level: Option<LogLevel>,
    trace_context_provider: Option<TraceContextProvider>,
    tool_call_policy: Option<Box<dyn ToolCallPolicy>>,
}

/// Builder for configuring and connecting an [`McpClient`].
///
/// Most settings here are settled before the connection opens, because they
/// describe what the client declares during the handshake. Per-request
/// defaults may instead seed runtime-mutable client state. Declaring a
/// capability is a promise to answer the matching server request, so pair
/// [`with_sampling`](Self::with_sampling) and
/// [`with_elicitation`](Self::with_elicitation) with a [`ClientHandler`] that
/// implements them; a client that declares one and connects with
/// [`connect_simple`](Self::connect_simple) advertises something it will
/// refuse.
///
/// # Example
///
/// ```rust
/// use tower_mcp::client::{ChannelTransport, McpClient};
/// use tower_mcp::protocol::Root;
/// use tower_mcp::McpRouter;
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), tower_mcp::BoxError> {
/// # let transport = ChannelTransport::new(McpRouter::new().server_info("server", "1.0.0"));
/// // A handler-free client: it declares roots, which it answers itself, but
/// // nothing that needs a ClientHandler.
/// let client = McpClient::builder()
///     .with_roots(vec![Root::new("file:///project")])
///     .connect_simple(transport)
///     .await?;
///
/// client.initialize("my-client", "1.0.0").await?;
/// assert_eq!(client.roots().await.len(), 1);
///
/// // Roots stay editable, and the server is told when they change.
/// client.add_root(Root::new("file:///scratch")).await?;
/// assert_eq!(client.roots().await.len(), 2);
/// # client.shutdown().await?;
/// # Ok(())
/// # }
/// ```
pub struct McpClientBuilder {
    capabilities: ClientCapabilities,
    roots: Vec<Root>,
    protocol_support: ProtocolSupport,
    max_mrtr_rounds: usize,
    max_list_pages: usize,
    cache_config: ClientCacheConfig,
    request_progress: bool,
    request_log_level: Option<LogLevel>,
    trace_context_provider: Option<TraceContextProvider>,
    tool_call_policy: Option<Box<dyn ToolCallPolicy>>,
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
            max_list_pages: DEFAULT_MAX_LIST_PAGES,
            cache_config: ClientCacheConfig::default(),
            request_progress: false,
            request_log_level: None,
            trace_context_provider: None,
            tool_call_policy: None,
        }
    }

    /// Install an asynchronous policy for outbound `tools/call` requests.
    ///
    /// The policy is a client-side control. Servers remain responsible for
    /// authorization, and sandboxing must be provided separately. Server tool
    /// annotations carry no authority or guarantee about replay safety.
    ///
    /// | Path | Behavior with a policy |
    /// | --- | --- |
    /// | `call_tool`, `call_tool_text`, `call_tool_once`, `call_tool_once_task_aware`, `call_tool_as_task` | Check each outgoing `tools/call`. |
    /// | `request("tools/call", ...)` | Check valid requests. Reject malformed parameters and a non-null `task` on 2026-07-28 locally. |
    /// | `notify("tools/call", ...)` | Reject locally because `tools/call` requires a request ID. |
    /// | MRTR, stale-schema retry, and legacy session replay | Check each new attempt. |
    /// | Task lifecycle RPCs, other methods, and server callbacks | Outside the tool-call policy. |
    /// | Transport-level HTTP redirects and OAuth challenge resends | Re-deliver the request as part of the same approved dispatch without another check. |
    /// | Direct [`ClientTransport::send`] | Bypasses the client policy. |
    ///
    /// Decisions apply to one dispatch. On legacy session expiry before
    /// dispatch, the client returns [`Error::SessionExpired`] or recovers and
    /// asks the policy again if recovery is enabled. If a session reset has
    /// already changed the generation, the old decision can fail with
    /// [`Error::ToolCallContextChanged`]. When a session expires while a
    /// decision is pending, which of the two the caller sees depends on
    /// whether recovery finished before the decision returned. In both cases
    /// nothing was sent.
    ///
    /// The generation tracks legacy session resets, including failed
    /// `initialize` cleanup. The 2026-07-28 lifecycle is sessionless.
    /// Tool-list changes, repeated `discover` or `initialize` calls without
    /// a reset, and a different server behind the same final-protocol endpoint
    /// leave pending decisions in effect. Callers must track any tool or
    /// server identity changes that matter to their policy.
    ///
    /// ```rust
    /// use tower_mcp::{McpClient, ToolCallContext, ToolCallDecision, ToolCallPolicy};
    ///
    /// struct ListOnly;
    ///
    /// #[async_trait::async_trait]
    /// impl ToolCallPolicy for ListOnly {
    ///     async fn authorize(&self, call: ToolCallContext<'_>) -> ToolCallDecision {
    ///         match call.name {
    ///             "files/list" => ToolCallDecision::Allow,
    ///             _ => ToolCallDecision::Deny {
    ///                 reason: "Only files/list is enabled".into(),
    ///             },
    ///         }
    ///     }
    /// }
    ///
    /// let builder = McpClient::builder().tool_call_policy(ListOnly);
    /// ```
    pub fn tool_call_policy(mut self, policy: impl ToolCallPolicy + 'static) -> Self {
        self.tool_call_policy = Some(Box::new(policy));
        self
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

    /// Set the log threshold attached to final-protocol requests.
    ///
    /// This opts into the deprecated 2026-07-28 per-request logging mechanism:
    /// each request carries `io.modelcontextprotocol/logLevel`, allowing the
    /// server to emit matching `notifications/message` frames. The setting is
    /// ignored by stable lifecycles and does not send `logging/setLevel`.
    ///
    /// The value seeds the connected client and may later be changed or
    /// cleared with [`McpClient::set_request_log_level`]. It is off by default.
    pub fn request_log_level(mut self, level: LogLevel) -> Self {
        self.request_log_level = Some(level);
        self
    }

    /// Send W3C trace context (SEP-414) on every request.
    ///
    /// `provider` is called once for each outgoing request, on the task that
    /// makes the call, so it can read the caller's current `tracing` span or
    /// OpenTelemetry context. When it returns a [`TraceContext`], the request's
    /// `params._meta` gets `traceparent` and, if the context has one,
    /// `tracestate`. When it returns `None`, no trace keys are added.
    ///
    /// A request whose `_meta` already has `traceparent` or `tracestate` is
    /// left as the caller wrote it, because the two keys describe one trace
    /// and the provider's value is never mixed in. The hook
    /// applies to requests on both the `initialize` lifecycle and 2026-07-28,
    /// including `initialize`, `server/discover` and `subscriptions/listen`;
    /// notifications do not carry it. It works with no OpenTelemetry
    /// dependency in this crate: [`TraceContext::parse`] is the only
    /// constructor, so a value the provider cannot parse is `None` and nothing
    /// malformed is sent. See [`guides::tracing`](crate::guides::tracing).
    ///
    /// ```rust
    /// use tower_mcp::client::McpClient;
    /// use tower_mcp::TraceContext;
    ///
    /// let builder = McpClient::builder().trace_context_provider(|| {
    ///     TraceContext::parse(
    ///         "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
    ///         None,
    ///     )
    /// });
    /// # let _ = builder;
    /// ```
    ///
    /// [`TraceContext`]: crate::trace_context::TraceContext
    /// [`TraceContext::parse`]: crate::trace_context::TraceContext::parse
    pub fn trace_context_provider<F>(mut self, provider: F) -> Self
    where
        F: Fn() -> Option<crate::trace_context::TraceContext> + Send + Sync + 'static,
    {
        self.trace_context_provider = Some(Arc::new(provider));
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

    /// Bound the number of pages one automatic list walk may fetch.
    ///
    /// Applies to every `list_all_*` helper and its `_with_cache_hints`
    /// variant, for tools, resources, resource templates, prompts and skills.
    /// Each call counts its own pages. A walk that has fetched `pages` pages
    /// and is handed another `nextCursor` fails with
    /// [`Error::ListPageLimitExceeded`] instead of returning the partial
    /// result as if it were the whole catalog. A walk that ends in exactly
    /// `pages` pages succeeds.
    ///
    /// Zero is normalized to one. The default is
    /// [`DEFAULT_MAX_LIST_PAGES`].
    ///
    /// MCP cursors are opaque, so the client cannot tell a server that is
    /// looping (repeating a cursor, or minting a fresh one forever) from one
    /// with a very long catalog. A request timeout bounds each page but not a
    /// sequence of prompt, successful pages, so this budget is what stops the
    /// walk.
    ///
    /// This is a page count, not a memory limit. A server picks its own page
    /// size, so the items a walk collects are bounded only by the page budget
    /// times however many items each page carries. Per-response size limits
    /// belong to the transport (for example the HTTP client's maximum SSE
    /// event size, or the stdio maximum frame size) and are configured
    /// separately. A caller that cannot hold a full catalog in memory should
    /// page with the `list_*_with_cursor` methods instead.
    ///
    /// ```rust
    /// use tower_mcp::client::McpClient;
    ///
    /// let builder = McpClient::builder().max_list_pages(50);
    /// # let _ = builder;
    /// ```
    pub fn max_list_pages(mut self, pages: usize) -> Self {
        self.max_list_pages = pages.max(1);
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
    ///
    /// No MCP request has been sent when this returns. Call
    /// [`McpClient::initialize`] or [`McpClient::discover`] next.
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
                max_list_pages: self.max_list_pages,
                cache_config: self.cache_config,
                request_progress: self.request_progress,
                request_log_level: self.request_log_level,
                trace_context_provider: self.trace_context_provider,
                tool_call_policy: self.tool_call_policy,
            },
        )
        .await
    }

    /// Connect to a server without a handler.
    ///
    /// All server-initiated requests will be rejected with `method_not_found`.
    /// Two are answered by the loop itself and so still work: `ping`, and
    /// `roots/list` whenever [`with_roots`](Self::with_roots) configured a
    /// non-empty list.
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
            max_list_pages,
            cache_config,
            request_progress,
            request_log_level,
            trace_context_provider,
            tool_call_policy,
        } = settings;
        let supports_session_recovery = transport.supports_session_recovery();
        let (command_tx, command_rx) = mpsc::channel::<LoopCommand>(64);
        let connected = AtomicBool::new(true);
        let roots = Arc::new(RwLock::new(roots));
        let response_cache = ClientResponseCache::new(cache_config);

        let loop_roots = roots.clone();
        let loop_response_cache = response_cache.clone();
        let next_request_id = AtomicI64::new(1);
        let session = Arc::new(ClientSession {
            command_tx,
            initialized: AtomicBool::new(false),
            server_info: RwLock::new(None),
            capabilities,
            protocol_support,
            selected_protocol_version: RwLock::new(None),
            client_info: RwLock::new(None),
            connected,
            recovery_client_info: RwLock::new(None),
            recovery_lock: Mutex::new(()),
            generation: AtomicU64::new(0),
            expired: AtomicBool::new(false),
            tool_call_policy,
            progress_tokens: request_progress.then(|| Arc::new(AtomicI64::new(1))),
            request_log_level: RwLock::new(request_log_level),
            trace_context_provider,
            next_request_id,
        });
        let loop_session = session.clone();

        let task = tokio::spawn(async move {
            message_loop(
                transport,
                handler,
                command_rx,
                loop_session,
                loop_roots,
                loop_response_cache,
            )
            .await;
        });

        Ok(Self {
            session,
            task: Some(task),
            discovery: RwLock::new(None),
            roots,
            supports_session_recovery,
            max_mrtr_rounds,
            max_list_pages,
            response_cache,
        })
    }

    /// Check if the client has been initialized.
    pub fn is_initialized(&self) -> bool {
        self.session.initialized.load(Ordering::Acquire)
    }

    /// Check if the transport is still connected.
    pub fn is_connected(&self) -> bool {
        self.session.connected.load(Ordering::Acquire)
    }

    /// Get the server info (available after initialization).
    pub async fn server_info(&self) -> Option<InitializeResult> {
        self.session.server_info.read().await.clone()
    }

    /// Return the exact ordered protocol implementations enabled for this client.
    pub fn protocol_support(&self) -> &ProtocolSupport {
        &self.session.protocol_support
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

    /// Set the log threshold attached to future final-protocol requests.
    ///
    /// Passing `Some(level)` opts subsequently prepared 2026-07-28 requests
    /// into log delivery at that threshold. Passing `None` clears the default,
    /// so later requests omit `io.modelcontextprotocol/logLevel` again. Stable
    /// lifecycles are unaffected; this method does not send
    /// `logging/setLevel`.
    ///
    /// Each wire request snapshots the value while its metadata is prepared.
    /// Requests already serialized or in flight retain their previous value;
    /// later MRTR rounds, retries, task operations, and subscription requests
    /// take a fresh snapshot.
    pub async fn set_request_log_level(&self, level: Option<LogLevel>) {
        *self.session.request_log_level.write().await = level;
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
        self.session.selected_protocol_version.read().await.clone()
    }

    /// Get the server info synchronously (best-effort, non-blocking).
    ///
    /// Returns `None` if the lock is currently held by a writer or if
    /// initialization hasn't completed. Prefer [`server_info()`](Self::server_info)
    /// in async contexts.
    pub fn server_info_blocking(&self) -> Option<InitializeResult> {
        self.session.server_info.try_read().ok()?.clone()
    }

    /// Initialize the MCP connection.
    ///
    /// Sends the `initialize` request and `notifications/initialized`
    /// notification. Must be called before any other operations: everything
    /// else refuses to run until this has succeeded.
    ///
    /// This is the 2025-11-25 handshake. It always offers
    /// [`LATEST_PROTOCOL_VERSION`](crate::protocol::LATEST_PROTOCOL_VERSION)
    /// and lets the server choose; the sessionless 2026-07-28 lifecycle is
    /// [`discover`](Self::discover) instead, and the two are alternatives
    /// rather than steps.
    ///
    /// A failure to deliver `notifications/initialized` fails the whole call,
    /// even though the `initialize` request itself succeeded. The server
    /// rejects every later request until that notification lands, so reporting
    /// success here would leave a client that looks connected and works for
    /// nothing.
    ///
    /// The name and version are recorded, so a transport that supports session
    /// recovery can re-initialize with the same identity without the caller
    /// repeating it.
    ///
    /// If the handshake fails after the server issued a session, the session
    /// is ended (an HTTP transport sends the `DELETE`) instead of being left
    /// to idle out.
    pub async fn initialize(
        &self,
        client_name: &str,
        client_version: &str,
    ) -> Result<InitializeResult> {
        // A failure of a repeat call leaves the established session alone;
        // only a handshake that never completed has a session to release.
        let handshake = self.session.recovery_lock.lock().await;
        if self.session.expired.load(Ordering::Acquire) && self.supports_session_recovery {
            // A caller may have dropped the very first initialize before its
            // identity could be saved for automatic recovery.
            self.session.reset_transport(false).await?;
        }
        let generation = self.session.generation.load(Ordering::Acquire);
        let was_initialized = self.session.initialized.load(Ordering::Acquire);
        let client_info = Implementation {
            name: client_name.to_string(),
            version: client_version.to_string(),
            ..Default::default()
        };
        let result = self.session.initialize_once(client_info.clone()).await;
        if matches!(result, Err(Error::SessionExpired))
            && self.supports_session_recovery
            && self.session.expired.load(Ordering::Acquire)
        {
            // The server issued a session id but the listener lost it before
            // the first handshake completed. Its identity is already known.
            *self.session.recovery_client_info.write().await = Some(client_info);
            drop(handshake);
            self.session.await_recovery(generation).await?;
            return self.server_info().await.ok_or(Error::SessionExpired);
        }
        if result.is_err() && !was_initialized {
            self.session.end_abandoned_session().await;
        }
        result
    }
}

impl ClientSession {
    async fn initialize_once(&self, client_info: Implementation) -> Result<InitializeResult> {
        let params = InitializeParams {
            protocol_version: crate::protocol::LATEST_PROTOCOL_VERSION.to_string(),
            capabilities: self.capabilities.clone(),
            client_info,
            meta: None,
        };

        let result: InitializeResult = self
            .send_request_once(
                self.generation.load(Ordering::Acquire),
                "initialize",
                &params,
            )
            .await?;

        // The server picks the protocol version, not necessarily the one
        // offered. A response outside the initialize-lifecycle set this
        // client accepts -- a discover-lifecycle version like 2026-07-28, or
        // one the client narrowed itself away from via `ProtocolSupport` --
        // must not be treated as a successful handshake: nothing downstream
        // re-checks it, so an unvalidated version here silently drives every
        // later request with mismatched semantics.
        if !accepts_initialize_answer(&self.protocol_support, &result.protocol_version) {
            return Err(Error::Transport(format!(
                "server selected protocol version `{}`, which is not an initialize-lifecycle version this client supports; client supports: {:?}",
                result.protocol_version,
                self.protocol_support.versions()
            )));
        }

        // Send initialized notification. A delivery failure is an
        // initialization failure: the server will reject every subsequent
        // request until the notification arrives.
        self.send_notification("notifications/initialized", &serde_json::json!({}))
            .await
            .map_err(|error| {
                if matches!(error, Error::SessionExpired) {
                    error
                } else {
                    Error::Transport(format!(
                        "failed to deliver notifications/initialized: {error}"
                    ))
                }
            })?;
        if self.expired.load(Ordering::Acquire) {
            return Err(Error::SessionExpired);
        }
        *self.recovery_client_info.write().await = Some(params.client_info);
        *self.server_info.write().await = Some(result.clone());
        self.initialized.store(true, Ordering::Release);

        Ok(result)
    }
}

impl McpClient {
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
        *self.session.client_info.write().await = Some(client_info.clone());

        let mut candidate = self
            .session
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
                meta: Some(self.session.request_meta_for(&candidate, &client_info)),
            };
            let cache_key = serde_json::to_string(&(
                client_name,
                client_version,
                candidate.as_str(),
                &self.session.capabilities,
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
                    // `supported_versions` lists everything the server can
                    // speak, including legacy `initialize`-lifecycle
                    // versions it also accepts for backward compatibility.
                    // Restrict the pick to the discover lifecycle: selecting
                    // a legacy version here would leave the client believing
                    // it's on the sessionless 2026-07-28 path while every
                    // later request still carries per-request `_meta` the
                    // server never asked for.
                    let selected = self.session
                        .protocol_support
                        .versions()
                        .iter()
                        .find(|version| {
                            version.as_str() == PROTOCOL_VERSION_2026_07_28
                                && result
                                    .supported_versions
                                    .iter()
                                    .any(|supported| supported == *version)
                        })
                        .cloned()
                        .ok_or_else(|| {
                            Error::Transport(format!(
                                "server offers no mutually supported version for server/discover; server: {:?}, client: {:?}; fall back to initialize",
                                result.supported_versions,
                                self.session.protocol_support.versions()
                            ))
                        })?;
                    *self.session.selected_protocol_version.write().await = Some(selected);
                    *self.discovery.write().await = Some(result.clone());
                    self.session.initialized.store(true, Ordering::Release);
                    return Ok(result);
                }
                Err(Error::JsonRpc(error)) if error.code == -32022 && !retried_unsupported => {
                    let supported = error
                        .data
                        .as_ref()
                        .and_then(|data| data.get("supported"))
                        .and_then(serde_json::Value::as_array)
                        .ok_or_else(|| Error::JsonRpc(error.clone()))?;
                    // Only retry with a version that both uses the discover
                    // lifecycle and is enabled on this client. A legacy
                    // version in `supported` is not a `server/discover`
                    // candidate at all -- accepting it here would repeat the
                    // bug in the success branch above, just one round trip
                    // earlier.
                    candidate = self.session
                        .protocol_support
                        .versions()
                        .iter()
                        .find(|version| {
                            version.as_str() == PROTOCOL_VERSION_2026_07_28
                                && supported
                                    .iter()
                                    .any(|item| item.as_str() == Some(version.as_str()))
                        })
                        .cloned()
                        .ok_or_else(|| {
                            Error::Transport(format!(
                                "server offers no mutually supported version for server/discover; server: {supported:?}, client: {:?}; fall back to initialize",
                                self.session.protocol_support.versions()
                            ))
                        })?;
                    retried_unsupported = true;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// List available tools.
    ///
    /// One page. A server that paginates returns a `next_cursor`, and
    /// ignoring it silently hides the rest of the catalogue, so prefer
    /// [`list_all_tools`](Self::list_all_tools) unless the caller is driving
    /// pagination itself with
    /// [`list_tools_with_cursor`](Self::list_tools_with_cursor).
    pub async fn list_tools(&self) -> Result<ListToolsResult> {
        self.list_tools_with_cursor(None).await
    }

    /// Call a tool.
    ///
    /// Returns the tool's result whatever route the server took to produce it.
    /// If the server asks for input, the registered [`ClientHandler`] answers
    /// and the call is retried, up to
    /// [`max_mrtr_rounds`](McpClientBuilder::max_mrtr_rounds). If the server
    /// elects to run the call as a task, the task is polled to completion and
    /// its result returned here. Use
    /// [`call_tool_once`](Self::call_tool_once) or
    /// [`call_tool_once_task_aware`](Self::call_tool_once_task_aware) to drive
    /// either of those yourself.
    ///
    /// A result carrying `is_error: true` is returned as `Ok`, not `Err`: the
    /// tool ran and reported a domain error, which is an answer. `Err` is for
    /// a call that produced no result at all.
    ///
    /// On the final lifecycle, a header mismatch, method-not-found, or
    /// invalid-params response can indicate that the cached tool schema is
    /// stale. The client invalidates `tools/list`, refreshes it, and retries
    /// the rejected round once. These errors are raised before tool execution,
    /// so the bounded retry does not replay a completed side effect.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::client::{ChannelTransport, McpClient};
    /// use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct DivideInput {
    ///     numerator: i64,
    ///     denominator: i64,
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), tower_mcp::BoxError> {
    /// let divide = ToolBuilder::new("divide")
    ///     .description("Divide two integers")
    ///     .handler(|input: DivideInput| async move {
    ///         if input.denominator == 0 {
    ///             return Ok(CallToolResult::error("cannot divide by zero"));
    ///         }
    ///         Ok(CallToolResult::text(
    ///             (input.numerator / input.denominator).to_string(),
    ///         ))
    ///     })
    ///     .build();
    ///
    /// let server = McpRouter::new().server_info("math", "1.0.0").tool(divide);
    /// let client = McpClient::connect(ChannelTransport::new(server)).await?;
    /// client.initialize("my-client", "1.0.0").await?;
    ///
    /// let result = client
    ///     .call_tool("divide", serde_json::json!({"numerator": 9, "denominator": 3}))
    ///     .await?;
    /// assert!(!result.is_error);
    /// assert_eq!(result.all_text(), "3");
    ///
    /// // The tool's own error is a successful call with an error result, so
    /// // `?` does not short-circuit here.
    /// let refused = client
    ///     .call_tool("divide", serde_json::json!({"numerator": 9, "denominator": 0}))
    ///     .await?;
    /// assert!(refused.is_error);
    /// assert_eq!(refused.all_text(), "cannot divide by zero");
    /// # client.shutdown().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult> {
        let mut input_responses = None;
        let mut request_state = None;
        let generation = self.session.final_generation().await;
        let mut schema_retry_available = generation.is_some();
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
                    generation,
                )
                .await?;
            match outcome {
                TaskAwareCallToolOutcome::Complete(result) => return Ok(result),
                TaskAwareCallToolOutcome::Task(created) => {
                    return self
                        .complete_final_task(&created.task.metadata.task_id, generation)
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
                    input_responses =
                        Some(self.resolve_input_requests(generation, requests).await?);
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
        let mut schema_retry_available = self.session.uses_final_protocol().await;
        self.send_task_aware_tool_request_with_schema_retry(
            &params,
            &mut schema_retry_available,
            None,
        )
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
        if self.session.uses_final_protocol().await {
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
                    None,
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
        let mut schema_retry_available = self.session.uses_final_protocol().await;
        self.send_tool_request_with_schema_retry(&params, &mut schema_retry_available, None)
            .await
    }

    /// Fetch a task's current state via `tasks/get` (SEP-2663).
    ///
    /// For `completed` tasks the returned object carries the terminal
    /// [`CallToolResult`] in its `result` field; for `failed` tasks the
    /// JSON-RPC error is in `error`. Unknown or expired task ids surface as
    /// an invalid-params error from the server.
    ///
    /// A denied task reads exactly like an unknown one, so an
    /// invalid-params answer here does not distinguish "never existed",
    /// "expired", and "belongs to someone else". [`crate::async_task`]
    /// explains why the server refuses to tell them apart.
    pub async fn task_get(&self, task_id: &str) -> Result<TaskObject> {
        self.ensure_initialized()?;
        if self.session.uses_final_protocol().await {
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
        self.task_get_detailed_in_generation(task_id, None).await
    }

    async fn task_get_detailed_in_generation(
        &self,
        task_id: &str,
        generation: Option<u64>,
    ) -> Result<crate::tasks::GetTaskResult> {
        self.ensure_initialized()?;
        if !self.session.uses_final_protocol().await {
            return Err(Error::Transport(
                "task_get_detailed requires the 2026-07-28 client lifecycle".to_string(),
            ));
        }
        let params = crate::tasks::GetTaskParams {
            task_id: task_id.to_string(),
            meta: None,
        };
        self.send_request_in_generation(generation, "tasks/get", &params)
            .await
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
        if self.session.uses_final_protocol().await {
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
        self.task_update_in_generation(task_id, input_responses, None)
            .await
    }

    async fn task_update_in_generation(
        &self,
        task_id: &str,
        input_responses: InputResponses,
        generation: Option<u64>,
    ) -> Result<()> {
        self.ensure_initialized()?;
        if self.session.uses_final_protocol().await {
            let params = crate::tasks::UpdateTaskParams {
                task_id: task_id.to_string(),
                input_responses,
                meta: None,
            };
            let _ack: crate::tasks::UpdateTaskResult = self
                .send_request_in_generation(generation, "tasks/update", &params)
                .await?;
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
    ///
    /// This loops on its own: there is no upper bound beyond the task's TTL,
    /// and a task that keeps asking for input keeps the loop running. The
    /// push alternative is a
    /// [`listen_subscriptions`](Self::listen_subscriptions) stream naming the
    /// task, where each `notifications/tasks` carries the whole task and a
    /// completion arrives with its result already attached.
    pub async fn task_wait(&self, task_id: &str) -> Result<TaskObject> {
        let generation = self.session.final_generation().await;
        if generation.is_some() {
            let result = self.wait_for_final_task(task_id, generation).await?;
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
        let request_generation = self.session.final_generation().await;
        let cache_enabled = request_generation.is_some() && self.response_cache.enabled();
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
                .read_resource_in_generation(
                    uri,
                    input_responses.take(),
                    request_state.take(),
                    request_generation,
                )
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
                    input_responses = Some(
                        self.resolve_input_requests(request_generation, requests)
                            .await?,
                    );
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
        self.read_resource_in_generation(uri, input_responses, request_state, None)
            .await
    }

    async fn read_resource_in_generation(
        &self,
        uri: &str,
        input_responses: Option<InputResponses>,
        request_state: Option<String>,
        generation: Option<u64>,
    ) -> Result<RequestOutcome<ReadResourceResult>> {
        self.ensure_initialized()?;
        let params = ReadResourceParams {
            uri: uri.to_string(),
            input_responses,
            request_state,
            meta: None,
        };
        self.send_request_in_generation(generation, "resources/read", &params)
            .await
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
        if !self.session.uses_final_protocol().await {
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
        let params = self.session.with_final_request_meta(params).await?;
        let params = self.session.with_request_log_level(params).await;
        let params = self.session.with_trace_context(params);
        let (id_tx, id_rx) = oneshot::channel();
        let (acknowledgment_tx, acknowledgment_rx) = oneshot::channel();
        let (response_tx, response_rx) = oneshot::channel();
        self.session
            .command_tx
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
            command_tx: self.session.command_tx.clone(),
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
        if self.session.uses_final_protocol().await {
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
        if self.session.uses_final_protocol().await {
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
    ///
    /// Fails with [`Error::ListPageLimitExceeded`] if the server still names a
    /// next page after [`McpClientBuilder::max_list_pages`] pages.
    ///
    /// The pages are separate requests, so a catalogue that changes while the
    /// walk is in progress can yield a list that never existed at any single
    /// moment. That is inherent to cursor pagination rather than something
    /// this method could fix; a client that needs a consistent view should
    /// re-list after `notifications/tools/list_changed`.
    ///
    /// ```rust
    /// use tower_mcp::client::{ChannelTransport, McpClient};
    /// use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), tower_mcp::BoxError> {
    /// let mut server = McpRouter::new()
    ///     .server_info("many-tools", "1.0.0")
    ///     // One tool per page, so the walk has to follow a cursor.
    ///     .page_size(1);
    /// for name in ["alpha", "beta", "gamma"] {
    ///     server = server.tool(
    ///         ToolBuilder::new(name)
    ///             .description("A tool")
    ///             .no_params_handler(|| async { Ok(CallToolResult::text("ok")) })
    ///             .build(),
    ///     );
    /// }
    ///
    /// let client = McpClient::connect(ChannelTransport::new(server)).await?;
    /// client.initialize("my-client", "1.0.0").await?;
    ///
    /// let page = client.list_tools().await?;
    /// assert_eq!(page.tools.len(), 1);
    /// assert!(page.next_cursor.is_some(), "there is more to fetch");
    ///
    /// assert_eq!(client.list_all_tools().await?.len(), 3);
    /// # client.shutdown().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_all_tools(&self) -> Result<Vec<ToolDefinition>> {
        Ok(self.list_all_tools_with_cache_hints().await?.items)
    }

    /// List all tools with cache hints valid for the combined result.
    ///
    /// Use this when the caller keeps the combined list and needs to know how
    /// long it may reuse it, or whether it may share it across authorization
    /// contexts. Use [`list_all_tools`](Self::list_all_tools) when only the
    /// items matter. [`AggregateList`] documents how page hints combine.
    /// The page budget from [`McpClientBuilder::max_list_pages`] applies.
    ///
    /// ```rust
    /// use tower_mcp::client::{ChannelTransport, McpClient};
    /// use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), tower_mcp::BoxError> {
    /// let ping = ToolBuilder::new("ping")
    ///     .description("Reply with pong")
    ///     .no_params_handler(|| async { Ok(CallToolResult::text("pong")) })
    ///     .build();
    /// let server = McpRouter::new().server_info("demo", "1.0.0").tool(ping);
    /// let client = McpClient::connect(ChannelTransport::new(server)).await?;
    /// client.initialize("my-client", "1.0.0").await?;
    ///
    /// let listed = client.list_all_tools_with_cache_hints().await?;
    /// assert_eq!(listed.items.len(), 1);
    /// # client.shutdown().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_all_tools_with_cache_hints(&self) -> Result<AggregateList<ToolDefinition>> {
        self.collect_list_pages("tools/list", |cursor| async move {
            let page = self.list_tools_with_cursor(cursor).await?;
            Ok(ListPage {
                items: page.tools,
                next_cursor: page.next_cursor,
                ttl_ms: page.ttl_ms,
                cache_scope: page.cache_scope,
            })
        })
        .await
    }

    /// List all resources, following pagination cursors until exhausted.
    ///
    /// Fails with [`Error::ListPageLimitExceeded`] if the server still names a
    /// next page after [`McpClientBuilder::max_list_pages`] pages.
    pub async fn list_all_resources(&self) -> Result<Vec<ResourceDefinition>> {
        Ok(self.list_all_resources_with_cache_hints().await?.items)
    }

    /// List all resources with cache hints valid for the combined result.
    ///
    /// Use [`list_all_resources`](Self::list_all_resources) when only the items matter.
    /// [`AggregateList`] documents how page hints combine.
    /// The page budget from [`McpClientBuilder::max_list_pages`] applies.
    pub async fn list_all_resources_with_cache_hints(
        &self,
    ) -> Result<AggregateList<ResourceDefinition>> {
        self.collect_list_pages("resources/list", |cursor| async move {
            let page = self.list_resources_with_cursor(cursor).await?;
            Ok(ListPage {
                items: page.resources,
                next_cursor: page.next_cursor,
                ttl_ms: page.ttl_ms,
                cache_scope: page.cache_scope,
            })
        })
        .await
    }

    /// List all resource templates, following pagination cursors until exhausted.
    ///
    /// Fails with [`Error::ListPageLimitExceeded`] if the server still names a
    /// next page after [`McpClientBuilder::max_list_pages`] pages.
    pub async fn list_all_resource_templates(&self) -> Result<Vec<ResourceTemplateDefinition>> {
        Ok(self
            .list_all_resource_templates_with_cache_hints()
            .await?
            .items)
    }

    /// List all resource templates with cache hints valid for the combined result.
    ///
    /// Use [`list_all_resource_templates`](Self::list_all_resource_templates) when only the items matter.
    /// [`AggregateList`] documents how page hints combine.
    /// The page budget from [`McpClientBuilder::max_list_pages`] applies.
    pub async fn list_all_resource_templates_with_cache_hints(
        &self,
    ) -> Result<AggregateList<ResourceTemplateDefinition>> {
        self.list_resource_template_pages(false).await
    }

    #[cfg(feature = "proxy")]
    pub(crate) async fn list_optional_resource_templates(
        &self,
    ) -> Result<Vec<ResourceTemplateDefinition>> {
        Ok(self.list_resource_template_pages(true).await?.items)
    }

    async fn list_resource_template_pages(
        &self,
        allow_unsupported: bool,
    ) -> Result<AggregateList<ResourceTemplateDefinition>> {
        self.collect_list_pages("resources/templates/list", |cursor| async move {
            let first_page = cursor.is_none();
            let page = match self.list_resource_templates_with_cursor(cursor).await {
                Err(Error::JsonRpc(error))
                    if allow_unsupported
                        && first_page
                        && error.code == ErrorCode::MethodNotFound.code() =>
                {
                    return Ok(ListPage {
                        items: Vec::new(),
                        next_cursor: None,
                        ttl_ms: None,
                        cache_scope: None,
                    });
                }
                result => result?,
            };
            Ok(ListPage {
                items: page.resource_templates,
                next_cursor: page.next_cursor,
                ttl_ms: page.ttl_ms,
                cache_scope: page.cache_scope,
            })
        })
        .await
    }

    /// List all prompts, following pagination cursors until exhausted.
    ///
    /// Fails with [`Error::ListPageLimitExceeded`] if the server still names a
    /// next page after [`McpClientBuilder::max_list_pages`] pages.
    pub async fn list_all_prompts(&self) -> Result<Vec<PromptDefinition>> {
        Ok(self.list_all_prompts_with_cache_hints().await?.items)
    }

    /// List all prompts with cache hints valid for the combined result.
    ///
    /// Use [`list_all_prompts`](Self::list_all_prompts) when only the items matter.
    /// [`AggregateList`] documents how page hints combine.
    /// The page budget from [`McpClientBuilder::max_list_pages`] applies.
    pub async fn list_all_prompts_with_cache_hints(
        &self,
    ) -> Result<AggregateList<PromptDefinition>> {
        self.collect_list_pages("prompts/list", |cursor| async move {
            let page = self.list_prompts_with_cursor(cursor).await?;
            Ok(ListPage {
                items: page.prompts,
                next_cursor: page.next_cursor,
                ttl_ms: page.ttl_ms,
                cache_scope: page.cache_scope,
            })
        })
        .await
    }

    /// List skills served through the Skills extension (SEP-2640).
    ///
    /// Call this only against a server that declared
    /// `io.modelcontextprotocol/skills`; any other server answers
    /// `MethodNotFound`. An empty or partial listing does not mean the server
    /// has no other skills: [`get_skill`](Self::get_skill) answers for any
    /// skill it serves.
    pub async fn list_skills(&self) -> Result<crate::protocol::skills::ListSkillsResult> {
        self.list_skills_with_cursor(None).await
    }

    /// List skills with an optional pagination cursor.
    pub async fn list_skills_with_cursor(
        &self,
        cursor: Option<String>,
    ) -> Result<crate::protocol::skills::ListSkillsResult> {
        self.ensure_initialized()?;
        self.send_request(
            crate::protocol::skills::METHOD_SKILLS_LIST,
            &crate::protocol::skills::ListSkillsParams { cursor, meta: None },
        )
        .await
    }

    /// List all skills, following pagination cursors until exhausted.
    ///
    /// Fails with [`Error::ListPageLimitExceeded`] if the server still names a
    /// next page after [`McpClientBuilder::max_list_pages`] pages.
    pub async fn list_all_skills(&self) -> Result<Vec<crate::protocol::skills::Skill>> {
        Ok(self.list_all_skills_with_cache_hints().await?.items)
    }

    /// List all skills with cache hints valid for the combined result.
    ///
    /// Use [`list_all_skills`](Self::list_all_skills) when only the entries
    /// matter. [`AggregateList`] documents how page hints combine.
    /// The page budget from [`McpClientBuilder::max_list_pages`] applies.
    pub async fn list_all_skills_with_cache_hints(
        &self,
    ) -> Result<AggregateList<crate::protocol::skills::Skill>> {
        self.collect_list_pages(
            crate::protocol::skills::METHOD_SKILLS_LIST,
            |cursor| async move {
                let page = self.list_skills_with_cursor(cursor).await?;
                Ok(ListPage {
                    items: page.skills,
                    next_cursor: page.next_cursor,
                    ttl_ms: page.ttl_ms,
                    cache_scope: page.cache_scope,
                })
            },
        )
        .await
    }

    /// Fetch the entry for one skill by the URI of its `SKILL.md`.
    ///
    /// The server answers `-32602` (invalid params) when it serves no skill
    /// at that URI. Call this again to refresh an entry's digests, for
    /// example after a file fails verification.
    pub async fn get_skill(&self, uri: &str) -> Result<crate::protocol::skills::GetSkillResult> {
        self.ensure_initialized()?;
        self.send_request(
            crate::protocol::skills::METHOD_SKILLS_GET,
            &crate::protocol::skills::GetSkillParams {
                uri: uri.to_string(),
                meta: None,
            },
        )
        .await
    }

    /// List the direct children of a directory resource
    /// (`resources/directory/read`, SEP-2640).
    ///
    /// If the server's skills extension settings do not declare
    /// `directoryRead: true`, returns a local method-not-found error before
    /// sending a request. Subdirectories carry `mimeType: "inode/directory"`;
    /// the listing is not recursive.
    pub async fn read_resource_directory(
        &self,
        uri: &str,
        cursor: Option<String>,
    ) -> Result<crate::protocol::skills::ReadResourceDirectoryResult> {
        use crate::protocol::skills::{
            METHOD_RESOURCES_DIRECTORY_READ, ReadResourceDirectoryParams,
        };

        self.ensure_initialized()?;
        let params = ReadResourceDirectoryParams {
            uri: uri.to_string(),
            cursor,
            meta: None,
        };
        self.send_with_session_recovery(METHOD_RESOURCES_DIRECTORY_READ, |generation| {
            let params = &params;
            async move {
                self.ensure_directory_read_supported().await?;
                self.session
                    .send_request_once(generation, METHOD_RESOURCES_DIRECTORY_READ, params)
                    .await
            }
        })
        .await
    }

    async fn ensure_directory_read_supported(&self) -> Result<()> {
        use serde::Deserialize as _;

        use crate::protocol::skills::{
            METHOD_RESOURCES_DIRECTORY_READ, SKILLS_EXTENSION_ID, SkillsExtensionSettings,
        };

        let supports_directory_read = |capabilities: &crate::protocol::ServerCapabilities| {
            capabilities
                .extensions
                .as_ref()
                .and_then(|extensions| extensions.get(SKILLS_EXTENSION_ID))
                .and_then(|settings| SkillsExtensionSettings::deserialize(settings).ok())
                .is_some_and(|settings| settings.directory_read)
        };
        let supported = if self.session.uses_final_protocol().await {
            self.discovery
                .read()
                .await
                .as_ref()
                .is_some_and(|result| supports_directory_read(&result.capabilities))
        } else {
            self.session
                .server_info
                .read()
                .await
                .as_ref()
                .is_some_and(|result| supports_directory_read(&result.capabilities))
        };
        if !supported {
            return Err(Error::JsonRpc(JsonRpcError::method_not_found(
                METHOD_RESOURCES_DIRECTORY_READ,
            )));
        }
        Ok(())
    }

    /// Read one file of a skill and verify it against the skill's entry.
    ///
    /// This is the read-time verification SEP-2640 requires of hosts: the
    /// file must be listed in the entry's `resources`, its byte length must
    /// equal the listed `size`, and its SHA-256 digest must equal the listed
    /// `digest`. For the skill's `SKILL.md`, the parsed frontmatter must also
    /// equal the entry's `frontmatter`. A failure returns
    /// [`Error::Internal`](crate::Error) with the reason, and the content is
    /// not returned. On a mismatch, refresh the entry with
    /// [`get_skill`](Self::get_skill) before retrying.
    ///
    /// A dynamic skill (`resources: "dynamic"`) cannot be verified, so its
    /// files are rejected; read them with [`read_resource`](Self::read_resource)
    /// if the application accepts unverifiable content.
    #[cfg(feature = "skills")]
    pub async fn read_verified_skill_file(
        &self,
        skill: &crate::protocol::skills::Skill,
        uri: &str,
    ) -> Result<Vec<u8>> {
        crate::skills::verify::check_listed(skill, uri)
            .map_err(|e| Error::internal(e.to_string()))?;
        let result = self.read_resource(uri).await?;
        let content = result
            .contents
            .iter()
            .find(|c| c.uri == uri)
            .or_else(|| result.contents.first())
            .ok_or_else(|| {
                Error::internal(format!("resources/read of {uri} returned no content"))
            })?;
        let bytes = crate::skills::verify::content_bytes(content)
            .map_err(|e| Error::internal(e.to_string()))?;
        crate::skills::verify::verify_file(skill, uri, &bytes)
            .map_err(|e| Error::internal(e.to_string()))?;
        Ok(bytes)
    }

    /// Call a tool and return the concatenated text content.
    ///
    /// Returns the text from all [`Text`](crate::protocol::Content::Text) items joined together.
    /// If the tool result indicates an error (`is_error` is true), returns
    /// an error with the text content as the message.
    ///
    /// The convenience costs information in both directions. Non-text content
    /// (images, embedded resources, structured output) is dropped rather than
    /// rendered, and a tool's domain error arrives as `Err` here where
    /// [`call_tool`](Self::call_tool) reports it as a result, which makes it
    /// indistinguishable from a transport failure at the call site. Reach for
    /// this when a tool is known to answer in plain text and the caller treats
    /// any problem the same way.
    ///
    /// ```rust
    /// use tower_mcp::client::{ChannelTransport, McpClient};
    /// use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), tower_mcp::BoxError> {
    /// let motd = ToolBuilder::new("motd")
    ///     .description("Message of the day")
    ///     .no_params_handler(|| async { Ok(CallToolResult::text("be excellent")) })
    ///     .build();
    /// let refuse = ToolBuilder::new("refuse")
    ///     .description("Always reports a domain error")
    ///     .no_params_handler(|| async { Ok(CallToolResult::error("not today")) })
    ///     .build();
    ///
    /// let server = McpRouter::new()
    ///     .server_info("greeter", "1.0.0")
    ///     .tool(motd)
    ///     .tool(refuse);
    /// let client = McpClient::connect(ChannelTransport::new(server)).await?;
    /// client.initialize("my-client", "1.0.0").await?;
    ///
    /// assert_eq!(
    ///     client.call_tool_text("motd", serde_json::json!({})).await?,
    ///     "be excellent"
    /// );
    ///
    /// let error = client
    ///     .call_tool_text("refuse", serde_json::json!({}))
    ///     .await
    ///     .expect_err("an is_error result becomes an Err here");
    /// assert!(error.to_string().contains("not today"));
    /// # client.shutdown().await?;
    /// # Ok(())
    /// # }
    /// ```
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
        let generation = self.session.final_generation().await;
        let mut input_responses = None;
        let mut request_state = None;
        for round in 0..=self.max_mrtr_rounds {
            match self
                .get_prompt_in_generation(
                    name,
                    arguments.clone(),
                    input_responses.take(),
                    request_state.take(),
                    generation,
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
                    input_responses =
                        Some(self.resolve_input_requests(generation, requests).await?);
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
        self.get_prompt_in_generation(name, arguments, input_responses, request_state, None)
            .await
    }

    async fn get_prompt_in_generation(
        &self,
        name: &str,
        arguments: std::collections::HashMap<String, String>,
        input_responses: Option<InputResponses>,
        request_state: Option<String>,
        generation: Option<u64>,
    ) -> Result<RequestOutcome<GetPromptResult>> {
        self.ensure_initialized()?;
        let params = GetPromptParams {
            name: name.to_string(),
            arguments,
            input_responses,
            request_state,
            meta: None,
        };
        self.send_request_in_generation(generation, "prompts/get", &params)
            .await
    }

    /// Ping the server.
    pub async fn ping(&self) -> Result<()> {
        if self.session.uses_final_protocol().await {
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
    ///
    /// A `tools/call` request is subject to any configured
    /// [tool-call policy](McpClientBuilder::tool_call_policy).
    pub async fn request<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R> {
        self.send_request(method, params).await
    }

    /// Send a raw typed notification to the server.
    ///
    /// A client with a [tool-call policy](McpClientBuilder::tool_call_policy)
    /// rejects `tools/call` notifications locally.
    pub async fn notify<P: serde::Serialize>(&self, method: &str, params: &P) -> Result<()> {
        self.session.send_notification(method, params).await
    }

    /// Get the current roots.
    pub async fn roots(&self) -> Vec<Root> {
        self.roots.read().await.clone()
    }

    /// Set roots and notify the server if initialized.
    pub async fn set_roots(&self, roots: Vec<Root>) -> Result<()> {
        *self.roots.write().await = roots;
        if self.is_initialized() && !self.session.uses_final_protocol().await {
            self.session
                .send_notification(notifications::ROOTS_LIST_CHANGED, &serde_json::json!({}))
                .await?;
        }
        Ok(())
    }

    /// Add a root and notify the server if initialized.
    pub async fn add_root(&self, root: Root) -> Result<()> {
        self.roots.write().await.push(root);
        if self.is_initialized() && !self.session.uses_final_protocol().await {
            self.session
                .send_notification(notifications::ROOTS_LIST_CHANGED, &serde_json::json!({}))
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

        if removed && self.is_initialized() && !self.session.uses_final_protocol().await {
            self.session
                .send_notification(notifications::ROOTS_LIST_CHANGED, &serde_json::json!({}))
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
    ///
    /// Asks the message loop to stop and waits for it to finish, so the
    /// transport closes in an orderly way and a subprocess server sees its
    /// stdin end rather than a severed pipe.
    ///
    /// Dropping the client instead aborts the loop immediately. Any request
    /// still awaiting a response is abandoned, which is fine for a process
    /// that is exiting anyway and wrong for one that keeps running.
    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.session.command_tx.send(LoopCommand::Shutdown).await;
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
        generation: Option<u64>,
    ) -> Result<TaskAwareCallToolOutcome> {
        self.send_tool_request_with_schema_retry(params, retry_available, generation)
            .await
    }

    async fn wait_for_final_task(
        &self,
        task_id: &str,
        generation: Option<u64>,
    ) -> Result<crate::tasks::GetTaskResult> {
        loop {
            let task = self
                .task_get_detailed_in_generation(task_id, generation)
                .await?;
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
                    let responses = self.resolve_input_requests(generation, requests).await?;
                    self.task_update_in_generation(task_id, responses, generation)
                        .await?;
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

    async fn complete_final_task(
        &self,
        task_id: &str,
        generation: Option<u64>,
    ) -> Result<CallToolResult> {
        let task = self.wait_for_final_task(task_id, generation).await?;
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
        generation: Option<u64>,
    ) -> Result<R>
    where
        R: serde::de::DeserializeOwned,
    {
        match self
            .send_request_in_generation(generation, "tools/call", params)
            .await
        {
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
                self.send_request_in_generation(generation, "tools/call", params)
                    .await
            }
            result => result,
        }
    }

    /// Follow `nextCursor` from the first page until a page has none.
    ///
    /// Shared by every `list_all_*` helper. Fails with
    /// [`Error::ListPageLimitExceeded`] when `max_list_pages` pages have been
    /// fetched and the last still names a next page, discarding the partial
    /// result. Cursors are opaque, so an empty or repeated cursor is followed
    /// like any other and only the page count ends a walk the server never
    /// finishes.
    async fn collect_list_pages<T, F, Fut>(
        &self,
        method: &str,
        mut fetch: F,
    ) -> Result<AggregateList<T>>
    where
        F: FnMut(Option<String>) -> Fut,
        Fut: std::future::Future<Output = Result<ListPage<T>>>,
    {
        let mut all = Vec::new();
        let mut cursor = None;
        let mut hints = self.response_cache.aggregate_hints();
        let mut pages = 0usize;
        loop {
            let page = fetch(cursor.take()).await?;
            pages += 1;
            hints.observe(page.ttl_ms, page.cache_scope);
            all.extend(page.items);
            match page.next_cursor {
                None => return Ok(hints.finish(all)),
                Some(_) if pages >= self.max_list_pages => {
                    return Err(Error::ListPageLimitExceeded {
                        method: method.to_string(),
                        limit: self.max_list_pages,
                    });
                }
                Some(next) => cursor = Some(next),
            }
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
        let cache_allowed = self.session.uses_final_protocol().await;
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
        self.send_with_session_recovery(method, |generation| {
            self.session.send_request_once(generation, method, params)
        })
        .await
    }

    // Automatic final-protocol input rounds carry state from the response
    // that started them. Keep using that generation when sending it back.
    async fn send_request_in_generation<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        generation: Option<u64>,
        method: &str,
        params: &P,
    ) -> Result<R> {
        match generation {
            Some(generation) => {
                self.session
                    .send_request_once(generation, method, params)
                    .await
            }
            None => self.send_request(method, params).await,
        }
    }

    async fn send_with_session_recovery<R, F>(
        &self,
        method: &str,
        send: impl Fn(u64) -> F,
    ) -> Result<R>
    where
        F: std::future::Future<Output = Result<R>>,
    {
        let final_protocol = self.session.uses_final_protocol().await;
        let generation = self.session.generation.load(Ordering::Acquire);
        match send(generation).await {
            Err(Error::SessionExpired)
                if self.supports_session_recovery && !final_protocol && method != "initialize" =>
            {
                tracing::info!(method = %method, "Session expired, attempting recovery");
                self.session.await_recovery(generation).await?;
                send(self.session.generation.load(Ordering::Acquire)).await
            }
            other => other,
        }
    }
}

impl ClientSession {
    async fn send_request_once<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        generation: u64,
        method: &str,
        params: &P,
    ) -> Result<R> {
        self.ensure_connected()?;
        let params_value = serde_json::to_value(params)
            .map_err(|e| Error::Transport(format!("Failed to serialize params: {}", e)))?;
        let params_value = self.with_final_request_meta(params_value).await?;
        let params_value = self.with_request_log_level(params_value).await;
        let params_value = self.with_progress_token(params_value);
        let params_value = self.with_trace_context(params_value);

        if method == "tools/call"
            && let Some(policy) = &self.tool_call_policy
        {
            self.authorize_tool_call(policy.as_ref(), generation, &params_value)
                .await?;
        }

        let id = RequestId::Number(self.next_request_id.fetch_add(1, Ordering::Relaxed));
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(LoopCommand::Request {
                generation,
                id: id.clone(),
                method: method.to_string(),
                params: params_value,
                response_tx,
            })
            .await
            .map_err(|_| Error::Transport("Connection closed".to_string()))?;

        // Dropping the returned future is what a Rust caller does to abandon
        // a call: a `select!` losing a race, a timeout, a Ctrl-C handler. The
        // server was never told, so it kept working on an answer nobody was
        // waiting for. This guard tells it (#1202), and disarms once the
        // response arrives so a completed request is never cancelled.
        let mut guard = CancelOnDrop {
            id,
            command_tx: self.command_tx.clone(),
            armed: true,
        };

        let result = response_rx
            .await
            .map_err(|_| Error::Transport("Connection closed".to_string()));
        guard.armed = false;
        let result = result??;

        serde_json::from_value(result)
            .map_err(|e| Error::Transport(format!("Failed to deserialize response: {}", e)))
    }

    async fn authorize_tool_call(
        &self,
        policy: &dyn ToolCallPolicy,
        generation: u64,
        params: &serde_json::Value,
    ) -> Result<()> {
        let name = params
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| Error::invalid_params("tools/call requires a non-empty string name"))?;
        let task_augmented = CallToolParams::deserialize(params)
            .map_err(|_| Error::invalid_params("Malformed tools/call parameters"))?
            .task
            .is_some();
        let selected = self.selected_protocol_version.read().await.clone();
        let protocol_version = match selected {
            Some(version) => Some(version),
            None => self
                .server_info
                .read()
                .await
                .as_ref()
                .map(|server| server.protocol_version.clone()),
        };
        let context = ToolCallContext::new(name, params, protocol_version.as_deref(), generation);
        if task_augmented && !context.task_augmented {
            return Err(Error::invalid_params("tools/call task must be an object"));
        }
        if task_augmented
            && context.protocol_version == Some(crate::protocol::PROTOCOL_VERSION_2026_07_28)
        {
            return Err(Error::invalid_params(
                "The final Tasks extension does not allow a 'task' request parameter",
            ));
        }
        self.check_tool_call_session(generation)?;
        let decision = policy.authorize(context).await;
        self.check_tool_call_session(generation)?;

        match decision {
            ToolCallDecision::Allow => Ok(()),
            ToolCallDecision::Deny { reason } => Err(Error::ToolCallDenied {
                name: name.to_owned(),
                reason,
            }),
        }
    }

    fn check_tool_call_session(&self, generation: u64) -> Result<()> {
        if self.expired.load(Ordering::Acquire) {
            return Err(Error::SessionExpired);
        }
        if generation != self.generation.load(Ordering::Acquire) {
            return Err(Error::ToolCallContextChanged);
        }
        Ok(())
    }

    // Own the handshake independently of the caller. Dropping a request while
    // reset or initialize is in progress must still leave a usable session.
    fn start_recovery(self: &Arc<Self>, generation: u64) -> JoinHandle<Result<()>> {
        let session = self.clone();
        tokio::spawn(async move {
            let result = session.recover_session(generation).await;
            if let Err(error) = &result {
                tracing::warn!(%error, "Session recovery failed");
            }
            result
        })
    }

    async fn await_recovery(self: &Arc<Self>, generation: u64) -> Result<()> {
        self.start_recovery(generation)
            .await
            .map_err(|error| Error::Transport(format!("Session recovery stopped: {error}")))?
    }

    /// End outstanding work as soon as the listener reports the loss. The
    /// replacement handshake runs independently and can take considerably longer.
    fn session_lost(
        self: &Arc<Self>,
        pending: &mut HashMap<RequestId, PendingRequest>,
        recover: bool,
    ) {
        if self.expired.swap(true, Ordering::AcqRel) {
            return;
        }
        self.initialized.store(false, Ordering::Release);
        for (_, request) in pending.drain() {
            let error = if request.method == "initialize" {
                Error::SessionExpired
            } else {
                Error::Transport(
                    "Session expired while the request was in flight; its outcome is unknown"
                        .into(),
                )
            };
            let _ = request.response_tx.send(Err(error));
        }
        if recover {
            drop(self.start_recovery(self.generation.load(Ordering::Acquire)));
        }
    }

    /// Recover from a session expiry by resetting the transport and re-initializing.
    async fn recover_session(&self, generation: u64) -> Result<()> {
        // Serialize recovery attempts
        let _guard = self.recovery_lock.lock().await;

        if generation != self.generation.load(Ordering::Acquire) {
            return if self.initialized.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(Error::SessionExpired)
            };
        }

        let client_info = self
            .recovery_client_info
            .read()
            .await
            .clone()
            .ok_or_else(|| Error::Transport("Cannot recover: never initialized".to_string()))?;

        self.reset_transport(false).await?;

        tracing::info!("Re-initializing session after expiry");
        let result = self.initialize_once(client_info).await;
        if result.is_err() {
            self.end_abandoned_session().await;
        }
        result.map(|_| ())
    }

    /// Release the session a failed `initialize` left on the server.
    ///
    /// The server may have issued a session id before the handshake failed,
    /// and nothing else would end it short of the server's idle timeout.
    /// Best effort: a message loop that is already gone has nothing to end.
    async fn end_abandoned_session(&self) {
        let _ = self.reset_transport(true).await;
    }

    async fn reset_transport(&self, end_on_server: bool) -> Result<()> {
        let (done_tx, done_rx) = oneshot::channel();
        self.command_tx
            .send(LoopCommand::ResetSession {
                end_on_server,
                done_tx,
            })
            .await
            .map_err(|_| Error::Transport("Connection closed".into()))?;
        done_rx
            .await
            .map_err(|_| Error::Transport("Connection closed during session reset".into()))
    }

    async fn send_notification<P: serde::Serialize>(&self, method: &str, params: &P) -> Result<()> {
        if method == "tools/call" && self.tool_call_policy.is_some() {
            return Err(Error::JsonRpc(JsonRpcError::invalid_request(
                "tools/call must be sent as a request",
            )));
        }
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
                generation: self.generation.load(Ordering::Acquire),
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
}

impl McpClient {
    async fn resolve_input_requests(
        &self,
        generation: Option<u64>,
        requests: InputRequests,
    ) -> Result<InputResponses> {
        let Some(generation) = generation else {
            return Err(Error::Transport(
                "input_required results require the 2026-07-28 client lifecycle".to_string(),
            ));
        };
        for request in requests.values() {
            let declared = match request {
                InputRequest::CreateMessage(_) => self.session.capabilities.sampling.is_some(),
                InputRequest::ListRoots(_) => self.session.capabilities.roots.is_some(),
                InputRequest::Elicit(_) => self.session.capabilities.elicitation.is_some(),
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
        self.session
            .command_tx
            .send(LoopCommand::ResolveInputs {
                generation,
                requests,
                response_tx,
            })
            .await
            .map_err(|_| Error::Transport("Connection closed".to_string()))?;
        response_rx.await.map_err(|_| input_resolution_stopped())?
    }
}

impl ClientSession {
    async fn uses_final_protocol(&self) -> bool {
        self.selected_protocol_version.read().await.as_deref()
            == Some(crate::protocol::PROTOCOL_VERSION_2026_07_28)
    }

    // Pin automatic operations before their first request, including all
    // task polls, so any inputs they receive belong to the same generation.
    async fn final_generation(&self) -> Option<u64> {
        self.uses_final_protocol()
            .await
            .then(|| self.generation.load(Ordering::Acquire))
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

    /// Attach the provider's W3C trace context (SEP-414) to one request.
    ///
    /// Applied beside [`with_progress_token`](Self::with_progress_token), at
    /// the point every request funnels through, so it covers both lifecycles.
    /// It runs on the caller's task, which is what lets a provider read the
    /// caller's current span. A caller that set `traceparent` or `tracestate`
    /// in `_meta` wins outright, and notifications never pass through here.
    fn with_trace_context(&self, mut params: serde_json::Value) -> serde_json::Value {
        let Some(provider) = &self.trace_context_provider else {
            return params;
        };
        let Some(trace_context) = provider() else {
            return params;
        };
        // A null params value (a request with no arguments) becomes an empty
        // object so it can carry `_meta`; a positional value has nowhere to
        // put one and is left alone.
        if params.is_null() {
            params = serde_json::json!({});
        }
        let Some(object) = params.as_object_mut() else {
            return params;
        };
        let meta = object
            .entry("_meta")
            .or_insert_with(|| serde_json::json!({}));
        let Some(meta) = meta.as_object_mut() else {
            return params;
        };
        // The two keys describe one trace, so a caller that set either keeps
        // control of both rather than getting a mixed pair.
        if meta.contains_key("traceparent") || meta.contains_key("tracestate") {
            return params;
        }
        for (key, value) in trace_context.entries() {
            meta.insert(key.to_string(), serde_json::Value::String(value));
        }
        params
    }

    /// Attach the client's default log threshold to one final-protocol request.
    ///
    /// This runs after required final metadata has been assembled, including
    /// on the first `server/discover` request. An explicit per-call level wins.
    /// Notifications intentionally do not pass through this helper because
    /// `logLevel` belongs to request metadata.
    async fn with_request_log_level(&self, mut params: serde_json::Value) -> serde_json::Value {
        const PROTOCOL_VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
        const LOG_LEVEL_KEY: &str = "io.modelcontextprotocol/logLevel";

        let should_attach = params
            .get("_meta")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|meta| {
                meta.get(PROTOCOL_VERSION_KEY)
                    .and_then(serde_json::Value::as_str)
                    == Some(crate::protocol::PROTOCOL_VERSION_2026_07_28)
                    && !meta.contains_key(LOG_LEVEL_KEY)
            });
        if !should_attach {
            return params;
        }

        let Some(level) = *self.request_log_level.read().await else {
            return params;
        };
        let meta = params
            .get_mut("_meta")
            .and_then(serde_json::Value::as_object_mut)
            .expect("request metadata was validated as an object");
        meta.insert(
            LOG_LEVEL_KEY.to_string(),
            serde_json::to_value(level).expect("LogLevel is serializable"),
        );
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
}

impl McpClient {
    fn ensure_initialized(&self) -> Result<()> {
        if !self.session.initialized.load(Ordering::Acquire) {
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

/// Version answered by servers that predate 2025-03-26.
///
/// Many stdio servers still answer it (#1544). A tower-mcp server never
/// negotiates it, so it is not in `SUPPORTED_PROTOCOL_VERSIONS` or any
/// `ProtocolSupport`.
const LEGACY_INITIALIZE_VERSION: &str = "2024-11-05";

/// Whether `initialize` may complete with the version the server selected.
///
/// A `2024-11-05` answer is accepted as part of the oldest compatibility
/// band, so it follows `2025-03-26`: every built-in policy accepts it, and a
/// client narrowed away from `2025-03-26` rejects it.
fn accepts_initialize_answer(support: &ProtocolSupport, version: &str) -> bool {
    if version == LEGACY_INITIALIZE_VERSION {
        return support.contains("2025-03-26");
    }
    crate::protocol::SUPPORTED_PROTOCOL_VERSIONS.contains(&version) && support.contains(version)
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

/// Outcome of a server-initiated request run on its own task (#1483),
/// relayed back to the message loop so it -- the sole owner of the
/// transport -- can send the reply.
struct ServerRequestOutcome {
    /// The server's original request id.
    id: RequestId,
    /// `None` when the request was cancelled: JSON-RPC forbids answering a
    /// cancelled request, so the loop sends nothing for this id.
    response: Option<serde_json::Value>,
}

/// Bundles the message loop's two id-keyed maps into one
/// [`handle_incoming`] argument. #1483 added
/// `pending_server_requests` alongside the existing `pending_requests`,
/// which would otherwise push that function past clippy's
/// `too_many_arguments` limit.
struct PendingState<'a> {
    session: &'a Arc<ClientSession>,
    pending_requests: &'a mut HashMap<RequestId, PendingRequest>,
    pending_server_requests: &'a mut HashMap<RequestId, crate::context::CancellationToken>,
}

// Cancels any input rounds from this generation, including when the loop is
// aborted and never reaches its normal cleanup.
#[derive(Default)]
struct InputResolutionScope(CancellationToken);

impl Drop for InputResolutionScope {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Background message loop that multiplexes incoming/outgoing messages.
async fn message_loop<T: ClientTransport, H: ClientHandler>(
    mut transport: T,
    handler: H,
    mut command_rx: mpsc::Receiver<LoopCommand>,
    session: Arc<ClientSession>,
    roots: Arc<RwLock<Vec<Root>>>,
    response_cache: Arc<ClientResponseCache>,
) {
    let handler = Arc::new(handler);
    let mut pending_requests: HashMap<RequestId, PendingRequest> = HashMap::new();
    // Cancellation tokens for server-initiated requests running on their
    // own tasks, keyed by the server's request id (#1483). Populated by
    // handle_incoming when the request arrives, cancelled from here when a
    // matching notifications/cancelled arrives, and removed once the
    // task's outcome comes back over server_request_tx.
    let mut pending_server_requests: HashMap<RequestId, crate::context::CancellationToken> =
        HashMap::new();
    let (mut server_request_tx, mut server_request_rx) = mpsc::channel::<ServerRequestOutcome>(32);
    let mut input_resolution = InputResolutionScope::default();

    loop {
        tokio::select! {
            // Commands from McpClient methods
            command = command_rx.recv() => {
                match command {
                    Some(LoopCommand::Request { generation, id, method, params, response_tx }) => {
                        // A queued request must not escape onto the wire between
                        // reset and the replacement handshake.
                        let expired = session.expired.load(Ordering::Acquire);
                        let generation_changed = generation != session.generation.load(Ordering::Acquire);
                        if expired
                            || generation_changed
                            || (generation > 0 && !session.initialized.load(Ordering::Acquire)
                                && method != "initialize" && method != "server/discover")
                        {
                            let has_policy = method == "tools/call" && session.tool_call_policy.is_some();
                            let error = match (has_policy, expired, generation_changed) {
                                (true, true, _) => Error::SessionExpired,
                                (true, false, true) => Error::ToolCallContextChanged,
                                _ => Error::Transport("Session expired before the request was sent".into()),
                            };
                            let _ = response_tx.send(Err(error));
                            continue;
                        }

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
                        let id = RequestId::Number(session.next_request_id.fetch_add(1, Ordering::Relaxed));
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
                        // Any in-flight request may be cancelled, not only a
                        // subscription (#1202). An id with nothing pending is
                        // a no-op: the response already arrived, or the caller
                        // cancelled twice.
                        let result = match pending_requests.remove(&request_id) {
                            Some(pending) => {
                                let subscription = pending.method == "subscriptions/listen";
                                let result = transport.cancel_request(&request_id).await;
                                if result.is_ok() {
                                    let reason = if subscription {
                                        "subscription cancelled"
                                    } else {
                                        "request cancelled"
                                    };
                                    let _ = pending
                                        .response_tx
                                        .send(Err(Error::Transport(reason.to_string())));
                                } else {
                                    // The notification did not go out, so the
                                    // request is still live on the server;
                                    // put it back rather than orphan it.
                                    pending_requests.insert(request_id.clone(), pending);
                                }
                                result
                            }
                            None => Ok(()),
                        };
                        if let Some(done_tx) = done_tx {
                            let _ = done_tx.send(result);
                        }
                    }
                    Some(LoopCommand::Notify { generation, method, params, done_tx }) => {
                        if generation != session.generation.load(Ordering::Acquire)
                            || session.expired.load(Ordering::Acquire)
                            || (session.generation.load(Ordering::Acquire) > 0
                                && !session.initialized.load(Ordering::Acquire)
                                && method != "notifications/initialized")
                        {
                            let _ = done_tx.send(Err(Error::SessionExpired));
                            continue;
                        }
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
                    Some(LoopCommand::ResolveInputs { generation, requests, response_tx }) => {
                        if response_tx.is_closed() {
                            continue;
                        }
                        if generation != session.generation.load(Ordering::Acquire)
                            || session.expired.load(Ordering::Acquire)
                        {
                            let _ = response_tx.send(Err(input_resolution_stopped()));
                            continue;
                        }
                        let token = input_resolution.0.child_token();
                        let handler = Arc::clone(&handler);
                        let roots = Arc::clone(&roots);
                        tokio::spawn(handler::scoped_input_round(
                            token.clone(),
                            resolve_inputs_with_handler(handler, roots, requests, token, response_tx),
                        ));
                    }
                    Some(LoopCommand::ResetSession { end_on_server, done_tx }) => {
                        tracing::info!(end_on_server, "Resetting transport session");
                        input_resolution = InputResolutionScope::default();
                        session.initialized.store(false, Ordering::Release);
                        *session.server_info.write().await = None;
                        session.generation.fetch_add(1, Ordering::AcqRel);
                        session.expired.store(false, Ordering::Release);
                        // These POSTs may already have executed. Only a POST explicitly
                        // rejected by the server is eligible for the caller's retry.
                        fail_all_pending(&mut pending_requests,
                            "Session expired while the request was in flight; its outcome is unknown");
                        for (_, token) in pending_server_requests.drain() {
                            token.cancel();
                        }
                        // An old handler can finish concurrently with cancellation.
                        // Its response and id must never reach the replacement session.
                        (server_request_tx, server_request_rx) = mpsc::channel(32);
                        if end_on_server {
                            transport.end_session().await;
                        } else {
                            transport.reset_session().await;
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
                            PendingState {
                                session: &session,
                                pending_requests: &mut pending_requests,
                                pending_server_requests: &mut pending_server_requests,
                            },
                            &handler,
                            &roots,
                            &mut transport,
                            &response_cache,
                            &server_request_tx,
                        ).await;
                    }
                    Ok(None) => {
                        tracing::info!("Transport closed (EOF)");
                        break;
                    }
                    Err(Error::SessionExpired) if !session.uses_final_protocol().await => {
                        let recover = transport.is_connected() && transport.supports_session_recovery();
                        session.session_lost(&mut pending_requests, recover);
                        if !recover {
                            session.initialized.store(false, Ordering::Release);
                            *session.server_info.write().await = None;
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Transport receive error");
                        break;
                    }
                }
            }

            // A server-initiated request's task finished or was cancelled
            // (#1483). The loop is the sole owner of the transport, so the
            // task hands its outcome back here instead of writing to it
            // directly; a cancelled request carries no response.
            Some(outcome) = server_request_rx.recv() => {
                pending_server_requests.remove(&outcome.id);
                if !session.expired.load(Ordering::Acquire)
                    && let Some(response) = outcome.response
                    && let Ok(json) = serde_json::to_string(&response)
                    && let Err(e) = transport.send(&json).await
                {
                    tracing::error!(error = %e, "Transport send error");
                    fail_all_pending(&mut pending_requests, &format!("Transport error: {}", e));
                    break;
                }
            }
        }
    }

    // Cleanup
    session.connected.store(false, Ordering::Release);
    drop(input_resolution);
    fail_all_pending(&mut pending_requests, "Connection closed");
    // Stop any server-initiated request tasks still running; their reply
    // would have nowhere to go now that the loop is exiting.
    for (_, token) in pending_server_requests.drain() {
        token.cancel();
    }
    let _ = transport.close().await;
    // A caller can take a queue slot before the loop stops and write its
    // command after. Dropping the receiver would strand that command, and its
    // reply channel, in the queue for as long as the client holds a sender.
    // Draining waits for outstanding slots, so every reply channel resolves.
    command_rx.close();
    while command_rx.recv().await.is_some() {}
}

const MAX_CONCURRENT_INPUT_KEYS: usize = 16;

async fn resolve_inputs_with_handler<H: ClientHandler>(
    handler: Arc<H>,
    roots: Arc<RwLock<Vec<Root>>>,
    requests: InputRequests,
    token: CancellationToken,
    mut response_tx: oneshot::Sender<Result<InputResponses>>,
) {
    let mut pending = FuturesUnordered::new();
    let result = {
        let mut requests = requests.into_iter();
        let mut responses = InputResponses::new();
        loop {
            while pending.len() < MAX_CONCURRENT_INPUT_KEYS {
                let Some((key, request)) = requests.next() else {
                    break;
                };
                let handler = handler.as_ref();
                let roots = roots.as_ref();
                let token = &token;
                pending.push(async move {
                    // A future already in the window may have never been polled.
                    if token.is_cancelled() {
                        return Err(input_resolution_stopped());
                    }
                    let response = resolve_input_with_handler(handler, roots, request).await?;
                    Ok((key, response))
                });
            }

            tokio::select! {
                biased;
                _ = response_tx.closed() => break Err(input_resolution_stopped()),
                _ = token.cancelled() => break Err(input_resolution_stopped()),
                outcome = pending.next() => match outcome {
                    Some(Ok((key, response))) => {
                        responses.insert(key, response);
                        tokio::task::consume_budget().await;
                    }
                    Some(Err(error)) => break Err(error),
                    None => break Ok(responses),
                },
            }
        }
    };

    if result.is_err() {
        token.cancel();
    }
    // Signal spawned work and drop pending handlers without waiting for them.
    drop(pending);
    if response_tx.send(result).is_err() {
        token.cancel();
    }
}

fn input_resolution_stopped() -> Error {
    Error::Transport("Input resolution stopped".to_string())
}

async fn resolve_input_with_handler<H: ClientHandler>(
    handler: &H,
    roots: &RwLock<Vec<Root>>,
    request: InputRequest,
) -> Result<InputResponse> {
    match request {
        InputRequest::CreateMessage(params) => handler
            .handle_create_message(params)
            .await
            .map(InputResponse::CreateMessage)
            .map_err(Error::JsonRpc),
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
            Ok(InputResponse::ListRoots(result))
        }
        InputRequest::Elicit(params) => handler
            .handle_elicit(params)
            .await
            .map(InputResponse::Elicit)
            .map_err(Error::JsonRpc),
        _ => Err(Error::Transport(
            "unsupported MRTR input request method".to_string(),
        )),
    }
}

/// Handle a single incoming message from the server.
async fn handle_incoming<T: ClientTransport, H: ClientHandler>(
    line: &str,
    state: PendingState<'_>,
    handler: &Arc<H>,
    roots: &Arc<RwLock<Vec<Root>>>,
    transport: &mut T,
    response_cache: &Arc<ClientResponseCache>,
    server_request_tx: &mpsc::Sender<ServerRequestOutcome>,
) {
    let PendingState {
        session,
        pending_requests,
        pending_server_requests,
    } = state;

    let parsed: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to parse incoming message");
            return;
        }
    };

    // Case 1: Response to one of our pending requests (has result or error, no method)
    if crate::framing::is_response_frame(&parsed) {
        // Check for session-level errors (id: null with -32005) that affect
        // all pending requests, not just a specific one.
        if let Some(error) = parsed.get("error") {
            let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0) as i32;
            let id_missing_or_null = parsed.get("id").is_none_or(|id| id.is_null());
            if code == -32005 && id_missing_or_null {
                tracing::warn!(
                    "Session expired (-32005 with null id), failing all pending requests"
                );
                if transport.supports_session_recovery() && !session.uses_final_protocol().await {
                    session.session_lost(pending_requests, true);
                } else {
                    for (_, pending) in pending_requests.drain() {
                        let _ = pending.response_tx.send(Err(Error::SessionExpired));
                    }
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
        let method = parsed["method"].as_str().unwrap_or("").to_string();
        let params = parsed.get("params").cloned();

        // MCP requires a request id to be a string or an integer, and
        // forbids null. A message with a `method` and any other id (a
        // float, a boolean, an object, an array, null) is an invalid
        // request, not a notification, so it is refused with -32600 and a
        // null id rather than dispatched or dropped (#1558). The handler
        // does not run: with no usable id there is nothing to key
        // cancellation bookkeeping on or correlate a reply to.
        let Some(id) = id else {
            let error = JsonRpcError::invalid_request("`id` must be a string or an integer");
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {
                    "code": error.code,
                    "message": error.message
                }
            });
            if let Ok(json) = serde_json::to_string(&response) {
                let _ = transport.send(&json).await;
            }
            return;
        };

        // Run on its own task, with its own CancellationToken, instead of
        // awaiting the handler inline: a notifications/cancelled for this
        // id needs to reach it before the handler returns, and a
        // ClientHandler that calls back into this same McpClient would
        // otherwise deadlock the message loop that is supposed to answer
        // it (#1483). Cancellation is cooperative, matching
        // CancellationToken's use everywhere else in this crate: the
        // handler can check or await the token via
        // `ClientHandler::current_cancellation_token` and read the wire ID
        // via `ClientHandler::current_request_id`. Once the handler returns,
        // a cancelled token suppresses its reply even if the handler ignored
        // the token. Otherwise `server_request_tx` relays the reply to the
        // message loop for sending over the transport.
        let token = crate::context::CancellationToken::new();
        pending_server_requests.insert(id.clone(), token.clone());

        let task_handler = Arc::clone(handler);
        let task_roots = Arc::clone(roots);
        let reply_tx = server_request_tx.clone();
        let task_id = id.clone();
        let task_token = token.clone();
        tokio::spawn(async move {
            let result = handler::scoped_server_request(
                task_id.clone(),
                task_token.clone(),
                dispatch_server_request(&task_handler, &task_roots, &method, params),
            )
            .await;
            let response = if task_token.is_cancelled() {
                None
            } else {
                Some(server_request_response(&task_id, result))
            };
            let _ = reply_tx
                .send(ServerRequestOutcome {
                    id: task_id,
                    response,
                })
                .await;
        });
        return;
    }

    // Case 3: Server notification (has method, no id)
    if parsed.get("method").is_some() && parsed.get("id").is_none() {
        let method = parsed["method"].as_str().unwrap_or("");
        let params = parsed.get("params").cloned();
        invalidate_response_cache(response_cache, method, params.as_ref()).await;
        let notification = parse_server_notification(method, params);

        // A notifications/cancelled might target a server-initiated
        // request this client is currently answering (sampling,
        // elicitation, roots, ping) rather than one of our own
        // subscriptions -- the wire shape is identical, so
        // parse_server_notification can't tell them apart. Check the
        // exact id first and hand off to that task without falling
        // through to the subscription-matching logic below, which owns a
        // separate id space. Exact HashMap lookup, not
        // matching_subscription_id's request_ids_match fallback: JSON-RPC
        // treats "1" and 1 as different ids (#1483).
        if let ServerNotification::SubscriptionCancelled {
            subscription_id, ..
        } = &notification
            && let Some(token) = pending_server_requests.get(subscription_id)
        {
            token.cancel();
            return;
        }

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

    // JSON-RPC 2.0 requires exactly one of `result`/`error`. A frame with
    // both would otherwise be treated as the server's error response
    // (`error` is checked first below); reject it as malformed instead. An
    // explicit `null` counts as absent, matching `JsonRpcResponse`.
    let error = parsed.get("error").filter(|error| !error.is_null());
    if error.is_some() && parsed.get("result").is_some_and(|result| !result.is_null()) {
        let _ = pending.response_tx.send(Err(Error::Transport(
            "Response has both result and error".to_string(),
        )));
        return;
    }

    if let Some(error) = error {
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

/// Build the JSON-RPC reply for a server-initiated request's outcome.
fn server_request_response(
    id: &RequestId,
    result: std::result::Result<serde_json::Value, JsonRpcError>,
) -> serde_json::Value {
    match result {
        Ok(value) => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": value
        }),
        Err(error) => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": error.code,
                "message": error.message
            }
        }),
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
    parsed.get("id").and_then(RequestId::from_json)
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
mod tests;
