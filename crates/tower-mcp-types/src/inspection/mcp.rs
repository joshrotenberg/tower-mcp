//! Exact-revision MCP method and parameter inspection.

use std::str::FromStr;

use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{JsonRpcEnvelope, JsonRpcInspectionError, JsonRpcPayload};
use crate::protocol::{
    CallToolParams, CancelTaskParams, CancelledParams, CompleteParams, CreateMessageParams,
    DiscoverParams, ElicitFormParams, ElicitRequestParams, ElicitationCompleteParams,
    GetPromptParams, GetTaskInfoParams, GetTaskResultParams, InitializeParams, ListPromptsParams,
    ListResourceTemplatesParams, ListResourcesParams, ListRootsParams, ListTasksParams,
    ListToolsParams, LoggingMessageParams, ProgressParams, ReadResourceParams, RequestMeta,
    SetLogLevelParams, SubscribeResourceParams, SubscriptionsAcknowledgedParams,
    SubscriptionsListenParams, TaskStatusParams, UnsubscribeResourceParams,
};

const REVISION_2025_03_26: u8 = 1 << 0;
const REVISION_2025_06_18: u8 = 1 << 1;
const REVISION_2025_11_25: u8 = 1 << 2;
const REVISION_2026_07_28: u8 = 1 << 3;
const LEGACY_REVISIONS: u8 = REVISION_2025_03_26 | REVISION_2025_06_18 | REVISION_2025_11_25;
const ALL_REVISIONS: u8 = LEGACY_REVISIONS | REVISION_2026_07_28;

/// Exact MCP revisions understood by [`McpInspector`], newest first.
///
/// This is a statement about the types crate's inspection profiles, not a
/// runtime allowlist. Applications must still explicitly choose which
/// revisions they accept.
pub const MCP_INSPECTION_PROFILES: &[&str] =
    &["2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26"];

/// An exact MCP wire revision understood by the semantic inspector.
///
/// There is intentionally no `Latest` variant. Adding a future profile does
/// not change an application's selected value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum McpProtocolRevision {
    /// The batch-capable March 2025 revision.
    V2025_03_26,
    /// The June 2025 revision that removed JSON-RPC batching.
    V2025_06_18,
    /// The November 2025 revision.
    V2025_11_25,
    /// The stateless July 2026 revision.
    V2026_07_28,
}

impl McpProtocolRevision {
    /// Return the exact date string used on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V2025_03_26 => "2025-03-26",
            Self::V2025_06_18 => "2025-06-18",
            Self::V2025_11_25 => "2025-11-25",
            Self::V2026_07_28 => "2026-07-28",
        }
    }

    const fn mask(self) -> u8 {
        match self {
            Self::V2025_03_26 => REVISION_2025_03_26,
            Self::V2025_06_18 => REVISION_2025_06_18,
            Self::V2025_11_25 => REVISION_2025_11_25,
            Self::V2026_07_28 => REVISION_2026_07_28,
        }
    }
}

impl std::fmt::Display for McpProtocolRevision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for McpProtocolRevision {
    type Err = McpInspectionError;

    fn from_str(revision: &str) -> Result<Self, Self::Err> {
        match revision {
            "2025-03-26" => Ok(Self::V2025_03_26),
            "2025-06-18" => Ok(Self::V2025_06_18),
            "2025-11-25" => Ok(Self::V2025_11_25),
            "2026-07-28" => Ok(Self::V2026_07_28),
            _ => Err(McpInspectionError::unsupported_profile(revision)),
        }
    }
}

impl TryFrom<&str> for McpProtocolRevision {
    type Error = McpInspectionError;

    fn try_from(revision: &str) -> Result<Self, Self::Error> {
        revision.parse()
    }
}

/// Sender and receiver roles for stateless method checks.
///
/// Direction does not establish capability negotiation, lifecycle state,
/// response correlation, or whether a referenced request is outstanding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpDirection {
    /// A client is sending a message to a server.
    ClientToServer,
    /// A server is sending a message to a client.
    ServerToClient,
}

impl std::fmt::Display for McpDirection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClientToServer => formatter.write_str("client-to-server"),
            Self::ServerToClient => formatter.write_str("server-to-client"),
        }
    }
}

/// Whether a method-bearing envelope is a request or notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpCallKind {
    /// A request with an ID.
    Request,
    /// A notification without an ID.
    Notification,
}

impl std::fmt::Display for McpCallKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request => formatter.write_str("request"),
            Self::Notification => formatter.write_str("notification"),
        }
    }
}

/// How an observed method relates to the selected MCP revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpMethodClassification {
    /// The method is defined for the selected revision and its params passed
    /// typed validation.
    Available,
    /// The crate knows this MCP method, but it is not a top-level method in
    /// the selected core revision.
    Unavailable,
    /// The method is not defined by any inspection profile and is left to an
    /// extension or embedding application.
    Extension,
}

/// Classification of one method-bearing envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpMethodInspection {
    method: String,
    kind: McpCallKind,
    classification: McpMethodClassification,
    batch_index: Option<usize>,
}

impl McpMethodInspection {
    /// Return the method exactly as observed on the wire.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Return whether this call was a request or notification.
    #[must_use]
    pub const fn kind(&self) -> McpCallKind {
        self.kind
    }

    /// Return how the method relates to the selected revision.
    #[must_use]
    pub const fn classification(&self) -> McpMethodClassification {
        self.classification
    }

    /// Return the zero-based batch index, or `None` for a single message.
    #[must_use]
    pub const fn batch_index(&self) -> Option<usize> {
        self.batch_index
    }
}

/// A structurally valid JSON-RPC payload inspected against one exact MCP
/// revision.
///
/// Method inspections are present for requests and notifications. Responses
/// have no method and therefore contribute no entry; correlating them to an
/// outstanding request remains an embedding concern.
#[derive(Debug, Clone)]
pub struct McpInspection {
    revision: McpProtocolRevision,
    direction: Option<McpDirection>,
    payload: JsonRpcPayload,
    methods: Vec<McpMethodInspection>,
}

impl McpInspection {
    /// Return the exact revision used for this operation.
    #[must_use]
    pub const fn revision(&self) -> McpProtocolRevision {
        self.revision
    }

    /// Return the optional role direction used for stateless checks.
    #[must_use]
    pub const fn direction(&self) -> Option<McpDirection> {
        self.direction
    }

    /// Borrow the structural JSON-RPC payload.
    #[must_use]
    pub const fn payload(&self) -> &JsonRpcPayload {
        &self.payload
    }

    /// Consume the inspection and return its structural JSON-RPC payload.
    #[must_use]
    pub fn into_payload(self) -> JsonRpcPayload {
        self.payload
    }

    /// Return request and notification classifications in wire order.
    #[must_use]
    pub fn methods(&self) -> &[McpMethodInspection] {
        &self.methods
    }
}

/// An immutable exact-revision MCP semantic inspector.
///
/// # Example
///
/// ```rust
/// use tower_mcp_types::inspection::{
///     McpDirection, McpInspector, McpMethodClassification,
/// };
///
/// let inspector = McpInspector::new("2025-11-25")?;
/// let value = serde_json::json!({
///     "jsonrpc": "2.0",
///     "id": 1,
///     "method": "tools/call",
///     "params": {"name": "weather", "arguments": {"city": "Seattle"}}
/// });
/// let inspected = inspector.inspect(&value, Some(McpDirection::ClientToServer))?;
/// assert_eq!(
///     inspected.methods()[0].classification(),
///     McpMethodClassification::Available,
/// );
/// # Ok::<(), tower_mcp_types::inspection::McpInspectionError>(())
/// ```
///
/// Unknown extension methods remain successful inspections. The caller keeps
/// the original [`Value`] for authorization and forwarding decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpInspector {
    revision: McpProtocolRevision,
}

impl McpInspector {
    /// Create an inspector for one exact date-based MCP revision.
    ///
    /// Unknown revisions return [`McpInspectionErrorKind::UnsupportedProfile`]
    /// rather than falling back to the newest known profile.
    pub fn new(revision: &str) -> Result<Self, McpInspectionError> {
        Ok(Self {
            revision: revision.parse()?,
        })
    }

    /// Create an inspector from an already parsed revision.
    #[must_use]
    pub const fn for_revision(revision: McpProtocolRevision) -> Self {
        Self { revision }
    }

    /// Return the exact immutable profile selected for this inspector.
    #[must_use]
    pub const fn revision(self) -> McpProtocolRevision {
        self.revision
    }

    /// Inspect a decoded JSON value structurally and then apply exact-revision
    /// MCP batch, method, direction, and parameter rules.
    ///
    /// Passing no direction skips role checks while retaining method-kind and
    /// parameter validation. Responses are structurally checked but cannot be
    /// semantically correlated without request state.
    pub fn inspect(
        self,
        value: &Value,
        direction: Option<McpDirection>,
    ) -> Result<McpInspection, McpInspectionError> {
        let payload = JsonRpcPayload::inspect(value)
            .map_err(|source| McpInspectionError::json_rpc(self.revision, source))?;
        self.inspect_payload(payload, direction)
    }

    /// Apply exact-revision MCP rules to an already inspected JSON-RPC
    /// payload.
    pub fn inspect_payload(
        self,
        payload: JsonRpcPayload,
        direction: Option<McpDirection>,
    ) -> Result<McpInspection, McpInspectionError> {
        if payload.is_batch() && self.revision != McpProtocolRevision::V2025_03_26 {
            return Err(McpInspectionError::new(
                McpInspectionErrorKind::BatchUnavailable,
                Some(self.revision.as_str()),
                None,
                None,
                format!(
                    "MCP {} does not permit top-level JSON-RPC batches",
                    self.revision
                ),
            ));
        }

        let mut methods = Vec::new();
        match &payload {
            JsonRpcPayload::Single(envelope) => {
                if let Some(method) = self.inspect_envelope(envelope, direction, None, false)? {
                    methods.push(method);
                }
            }
            JsonRpcPayload::Batch(batch) => {
                for (index, envelope) in batch.messages().iter().enumerate() {
                    if let Some(method) =
                        self.inspect_envelope(envelope, direction, Some(index), true)?
                    {
                        methods.push(method);
                    }
                }
            }
        }

        Ok(McpInspection {
            revision: self.revision,
            direction,
            payload,
            methods,
        })
    }

    fn inspect_envelope(
        self,
        envelope: &JsonRpcEnvelope,
        direction: Option<McpDirection>,
        batch_index: Option<usize>,
        in_batch: bool,
    ) -> Result<Option<McpMethodInspection>, McpInspectionError> {
        let (method, params, kind) = match envelope {
            JsonRpcEnvelope::Request(request) => (
                &request.method,
                request.params.as_ref(),
                McpCallKind::Request,
            ),
            JsonRpcEnvelope::Notification(notification) => (
                &notification.method,
                notification.params.as_ref(),
                McpCallKind::Notification,
            ),
            JsonRpcEnvelope::Result(_) | JsonRpcEnvelope::Error(_) => return Ok(None),
        };

        let Some(rule) = method_rule(method) else {
            return Ok(Some(McpMethodInspection {
                method: method.clone(),
                kind,
                classification: McpMethodClassification::Extension,
                batch_index,
            }));
        };

        if !rule.available_in(self.revision) {
            return Ok(Some(McpMethodInspection {
                method: method.clone(),
                kind,
                classification: McpMethodClassification::Unavailable,
                batch_index,
            }));
        }

        if kind != rule.kind {
            return Err(McpInspectionError::new(
                McpInspectionErrorKind::MessageKindMismatch,
                Some(self.revision.as_str()),
                Some(method),
                batch_index,
                format!(
                    "MCP {} defines `{method}` as a {}, not a {kind}",
                    self.revision, rule.kind
                ),
            ));
        }

        if let Some(direction) = direction
            && !rule.allows(self.revision, direction)
        {
            return Err(McpInspectionError::new(
                McpInspectionErrorKind::DirectionMismatch,
                Some(self.revision.as_str()),
                Some(method),
                batch_index,
                format!(
                    "MCP {} does not permit `{method}` in the {direction} direction",
                    self.revision
                ),
            ));
        }

        if in_batch && method == "initialize" {
            return Err(McpInspectionError::new(
                McpInspectionErrorKind::InitializeInBatch,
                Some(self.revision.as_str()),
                Some(method),
                batch_index,
                "MCP `initialize` must not be part of a JSON-RPC batch",
            ));
        }

        validate_params(*rule, self.revision, params).map_err(|failure| {
            McpInspectionError::new(
                failure.kind,
                Some(self.revision.as_str()),
                Some(method),
                batch_index,
                format!(
                    "invalid `{method}` params for MCP {}: {}",
                    self.revision, failure.detail
                ),
            )
        })?;

        Ok(Some(McpMethodInspection {
            method: method.clone(),
            kind,
            classification: McpMethodClassification::Available,
            batch_index,
        }))
    }
}

/// Stable category for an MCP semantic inspection failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpInspectionErrorKind {
    /// The requested exact inspection profile is not implemented.
    UnsupportedProfile,
    /// Structural JSON-RPC inspection failed first.
    JsonRpc,
    /// The selected MCP revision does not permit top-level batches.
    BatchUnavailable,
    /// The lifecycle `initialize` request appeared in a batch.
    InitializeInBatch,
    /// A known request method appeared as a notification or vice versa.
    MessageKindMismatch,
    /// A known method was sent by the wrong peer role.
    DirectionMismatch,
    /// A method that requires params omitted them.
    MissingParams,
    /// Present params did not match the selected revision's typed shape.
    InvalidParams,
}

/// A structured failure returned during exact-revision MCP inspection.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{detail}")]
#[non_exhaustive]
pub struct McpInspectionError {
    kind: McpInspectionErrorKind,
    revision: Option<String>,
    method: Option<String>,
    batch_index: Option<usize>,
    detail: String,
    #[source]
    source: Option<Box<JsonRpcInspectionError>>,
}

impl McpInspectionError {
    fn new(
        kind: McpInspectionErrorKind,
        revision: Option<&str>,
        method: Option<&str>,
        batch_index: Option<usize>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            revision: revision.map(str::to_owned),
            method: method.map(str::to_owned),
            batch_index,
            detail: detail.into(),
            source: None,
        }
    }

    fn unsupported_profile(revision: &str) -> Self {
        Self::new(
            McpInspectionErrorKind::UnsupportedProfile,
            Some(revision),
            None,
            None,
            format!(
                "unsupported MCP inspection profile `{revision}`; supported profiles: {}",
                MCP_INSPECTION_PROFILES.join(", ")
            ),
        )
    }

    fn json_rpc(revision: McpProtocolRevision, source: JsonRpcInspectionError) -> Self {
        Self {
            kind: McpInspectionErrorKind::JsonRpc,
            revision: Some(revision.as_str().to_string()),
            method: None,
            batch_index: source.batch_index(),
            detail: source.to_string(),
            source: Some(Box::new(source)),
        }
    }

    /// Return the stable error category for typed control flow.
    #[must_use]
    pub const fn kind(&self) -> McpInspectionErrorKind {
        self.kind
    }

    /// Return the selected or rejected revision, when applicable.
    #[must_use]
    pub fn revision(&self) -> Option<&str> {
        self.revision.as_deref()
    }

    /// Return the known method involved in the failure, when applicable.
    #[must_use]
    pub fn method(&self) -> Option<&str> {
        self.method.as_deref()
    }

    /// Return the zero-based batch-member index, when applicable.
    #[must_use]
    pub const fn batch_index(&self) -> Option<usize> {
        self.batch_index
    }

    /// Return the underlying structural JSON-RPC error, when structural
    /// inspection failed.
    #[must_use]
    pub fn json_rpc_error(&self) -> Option<&JsonRpcInspectionError> {
        self.source.as_deref()
    }

    /// Return the human-readable diagnostic.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

#[derive(Debug, Clone, Copy)]
struct MethodRule {
    name: &'static str,
    kind: McpCallKind,
    client_to_server: u8,
    server_to_client: u8,
    required_params: u8,
    validator: ParamsValidator,
}

impl MethodRule {
    const fn available_in(self, revision: McpProtocolRevision) -> bool {
        (self.client_to_server | self.server_to_client) & revision.mask() != 0
    }

    const fn allows(self, revision: McpProtocolRevision, direction: McpDirection) -> bool {
        let directions = match direction {
            McpDirection::ClientToServer => self.client_to_server,
            McpDirection::ServerToClient => self.server_to_client,
        };
        directions & revision.mask() != 0
    }

    const fn requires_params(self, revision: McpProtocolRevision) -> bool {
        self.required_params & revision.mask() != 0
    }
}

#[derive(Debug, Clone, Copy)]
enum ParamsValidator {
    Object,
    Initialize,
    Complete,
    SetLogLevel,
    GetPrompt,
    ListPrompts,
    ListResources,
    ListResourceTemplates,
    ReadResource,
    SubscribeResource,
    UnsubscribeResource,
    CallTool,
    ListTools,
    CreateMessage,
    ListRoots,
    Elicit,
    GetTask,
    GetTaskResult,
    ListTasks,
    CancelTask,
    Discover,
    SubscriptionsListen,
    Cancelled,
    Progress,
    LoggingMessage,
    ResourceUpdated,
    TaskStatus,
    ElicitationComplete,
    SubscriptionsAcknowledged,
}

const fn request(
    name: &'static str,
    client_to_server: u8,
    server_to_client: u8,
    required_params: u8,
    validator: ParamsValidator,
) -> MethodRule {
    MethodRule {
        name,
        kind: McpCallKind::Request,
        client_to_server,
        server_to_client,
        required_params,
        validator,
    }
}

const fn notification(
    name: &'static str,
    client_to_server: u8,
    server_to_client: u8,
    required_params: u8,
    validator: ParamsValidator,
) -> MethodRule {
    MethodRule {
        name,
        kind: McpCallKind::Notification,
        client_to_server,
        server_to_client,
        required_params,
        validator,
    }
}

// This table is derived from the official per-revision ClientRequest,
// ServerRequest, ClientNotification, and ServerNotification schema unions.
// A zero direction mask keeps an official extension method distinguishable
// from an unknown vendor extension without opting it into a core profile.
const METHOD_RULES: &[MethodRule] = &[
    request(
        "initialize",
        LEGACY_REVISIONS,
        0,
        LEGACY_REVISIONS,
        ParamsValidator::Initialize,
    ),
    request(
        "ping",
        LEGACY_REVISIONS,
        LEGACY_REVISIONS,
        0,
        ParamsValidator::Object,
    ),
    request(
        "completion/complete",
        ALL_REVISIONS,
        0,
        ALL_REVISIONS,
        ParamsValidator::Complete,
    ),
    request(
        "logging/setLevel",
        LEGACY_REVISIONS,
        0,
        LEGACY_REVISIONS,
        ParamsValidator::SetLogLevel,
    ),
    request(
        "prompts/get",
        ALL_REVISIONS,
        0,
        ALL_REVISIONS,
        ParamsValidator::GetPrompt,
    ),
    request(
        "prompts/list",
        ALL_REVISIONS,
        0,
        REVISION_2026_07_28,
        ParamsValidator::ListPrompts,
    ),
    request(
        "resources/list",
        ALL_REVISIONS,
        0,
        REVISION_2026_07_28,
        ParamsValidator::ListResources,
    ),
    request(
        "resources/templates/list",
        ALL_REVISIONS,
        0,
        REVISION_2026_07_28,
        ParamsValidator::ListResourceTemplates,
    ),
    request(
        "resources/read",
        ALL_REVISIONS,
        0,
        ALL_REVISIONS,
        ParamsValidator::ReadResource,
    ),
    request(
        "resources/subscribe",
        LEGACY_REVISIONS,
        0,
        LEGACY_REVISIONS,
        ParamsValidator::SubscribeResource,
    ),
    request(
        "resources/unsubscribe",
        LEGACY_REVISIONS,
        0,
        LEGACY_REVISIONS,
        ParamsValidator::UnsubscribeResource,
    ),
    request(
        "tools/call",
        ALL_REVISIONS,
        0,
        ALL_REVISIONS,
        ParamsValidator::CallTool,
    ),
    request(
        "tools/list",
        ALL_REVISIONS,
        0,
        REVISION_2026_07_28,
        ParamsValidator::ListTools,
    ),
    request(
        "sampling/createMessage",
        0,
        LEGACY_REVISIONS,
        LEGACY_REVISIONS,
        ParamsValidator::CreateMessage,
    ),
    request(
        "roots/list",
        0,
        LEGACY_REVISIONS,
        0,
        ParamsValidator::ListRoots,
    ),
    request(
        "elicitation/create",
        0,
        REVISION_2025_06_18 | REVISION_2025_11_25,
        REVISION_2025_06_18 | REVISION_2025_11_25,
        ParamsValidator::Elicit,
    ),
    request(
        "tasks/get",
        REVISION_2025_11_25,
        REVISION_2025_11_25,
        REVISION_2025_11_25,
        ParamsValidator::GetTask,
    ),
    request(
        "tasks/result",
        REVISION_2025_11_25,
        REVISION_2025_11_25,
        REVISION_2025_11_25,
        ParamsValidator::GetTaskResult,
    ),
    request(
        "tasks/list",
        REVISION_2025_11_25,
        REVISION_2025_11_25,
        0,
        ParamsValidator::ListTasks,
    ),
    request(
        "tasks/cancel",
        REVISION_2025_11_25,
        REVISION_2025_11_25,
        REVISION_2025_11_25,
        ParamsValidator::CancelTask,
    ),
    request("tasks/update", 0, 0, 0, ParamsValidator::Object),
    request(
        "server/discover",
        REVISION_2026_07_28,
        0,
        REVISION_2026_07_28,
        ParamsValidator::Discover,
    ),
    request(
        "subscriptions/listen",
        REVISION_2026_07_28,
        0,
        REVISION_2026_07_28,
        ParamsValidator::SubscriptionsListen,
    ),
    notification(
        "notifications/cancelled",
        ALL_REVISIONS,
        ALL_REVISIONS,
        ALL_REVISIONS,
        ParamsValidator::Cancelled,
    ),
    notification(
        "notifications/progress",
        LEGACY_REVISIONS,
        ALL_REVISIONS,
        ALL_REVISIONS,
        ParamsValidator::Progress,
    ),
    notification(
        "notifications/initialized",
        LEGACY_REVISIONS,
        0,
        0,
        ParamsValidator::Object,
    ),
    notification(
        "notifications/roots/list_changed",
        LEGACY_REVISIONS,
        0,
        0,
        ParamsValidator::Object,
    ),
    notification(
        "notifications/message",
        0,
        ALL_REVISIONS,
        ALL_REVISIONS,
        ParamsValidator::LoggingMessage,
    ),
    notification(
        "notifications/resources/updated",
        0,
        ALL_REVISIONS,
        ALL_REVISIONS,
        ParamsValidator::ResourceUpdated,
    ),
    notification(
        "notifications/resources/list_changed",
        0,
        ALL_REVISIONS,
        0,
        ParamsValidator::Object,
    ),
    notification(
        "notifications/tools/list_changed",
        0,
        ALL_REVISIONS,
        0,
        ParamsValidator::Object,
    ),
    notification(
        "notifications/prompts/list_changed",
        0,
        ALL_REVISIONS,
        0,
        ParamsValidator::Object,
    ),
    notification(
        "notifications/elicitation/complete",
        0,
        REVISION_2025_11_25,
        REVISION_2025_11_25,
        ParamsValidator::ElicitationComplete,
    ),
    notification(
        "notifications/tasks/status",
        REVISION_2025_11_25,
        REVISION_2025_11_25,
        REVISION_2025_11_25,
        ParamsValidator::TaskStatus,
    ),
    notification(
        "notifications/subscriptions/acknowledged",
        0,
        REVISION_2026_07_28,
        REVISION_2026_07_28,
        ParamsValidator::SubscriptionsAcknowledged,
    ),
];

fn method_rule(method: &str) -> Option<&'static MethodRule> {
    METHOD_RULES.iter().find(|rule| rule.name == method)
}

struct ParamsFailure {
    kind: McpInspectionErrorKind,
    detail: String,
}

impl ParamsFailure {
    fn missing() -> Self {
        Self {
            kind: McpInspectionErrorKind::MissingParams,
            detail: "required `params` field is missing".to_string(),
        }
    }

    fn invalid(detail: impl Into<String>) -> Self {
        Self {
            kind: McpInspectionErrorKind::InvalidParams,
            detail: detail.into(),
        }
    }
}

fn validate_params(
    rule: MethodRule,
    revision: McpProtocolRevision,
    params: Option<&Value>,
) -> Result<(), ParamsFailure> {
    if params.is_none() && rule.requires_params(revision) {
        return Err(ParamsFailure::missing());
    }

    let params = params
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    if !params.is_object() {
        return Err(ParamsFailure::invalid(
            "MCP method params must be a JSON object",
        ));
    }

    match rule.validator {
        ParamsValidator::Object => {}
        ParamsValidator::Initialize => decode::<InitializeParams>(&params)?,
        ParamsValidator::Complete => decode::<CompleteParams>(&params)?,
        ParamsValidator::SetLogLevel => decode::<SetLogLevelParams>(&params)?,
        ParamsValidator::GetPrompt => decode::<GetPromptParams>(&params)?,
        ParamsValidator::ListPrompts => decode::<ListPromptsParams>(&params)?,
        ParamsValidator::ListResources => decode::<ListResourcesParams>(&params)?,
        ParamsValidator::ListResourceTemplates => {
            decode::<ListResourceTemplatesParams>(&params)?;
        }
        ParamsValidator::ReadResource => decode::<ReadResourceParams>(&params)?,
        ParamsValidator::SubscribeResource => decode::<SubscribeResourceParams>(&params)?,
        ParamsValidator::UnsubscribeResource => decode::<UnsubscribeResourceParams>(&params)?,
        ParamsValidator::CallTool => decode::<CallToolParams>(&params)?,
        ParamsValidator::ListTools => decode::<ListToolsParams>(&params)?,
        ParamsValidator::CreateMessage => decode::<CreateMessageParams>(&params)?,
        ParamsValidator::ListRoots => decode::<ListRootsParams>(&params)?,
        ParamsValidator::Elicit => {
            if revision == McpProtocolRevision::V2025_06_18 {
                decode::<ElicitFormParams>(&params)?;
            } else {
                decode::<ElicitRequestParams>(&params)?;
            }
        }
        ParamsValidator::GetTask => decode::<GetTaskInfoParams>(&params)?,
        ParamsValidator::GetTaskResult => decode::<GetTaskResultParams>(&params)?,
        ParamsValidator::ListTasks => decode::<ListTasksParams>(&params)?,
        ParamsValidator::CancelTask => decode::<CancelTaskParams>(&params)?,
        ParamsValidator::Discover => decode::<DiscoverParams>(&params)?,
        ParamsValidator::SubscriptionsListen => {
            let parsed = decode_value::<SubscriptionsListenParams>(&params)?;
            if parsed.notifications.is_none() {
                return Err(ParamsFailure::invalid(
                    "required `notifications` field is missing",
                ));
            }
        }
        ParamsValidator::Cancelled => {
            let parsed = decode_value::<CancelledParams>(&params)?;
            if parsed.request_id.is_none() {
                return Err(ParamsFailure::invalid(
                    "required non-null `requestId` field is missing",
                ));
            }
        }
        ParamsValidator::Progress => decode::<ProgressParams>(&params)?,
        ParamsValidator::LoggingMessage => {
            if params.get("data").is_none() {
                return Err(ParamsFailure::invalid("required `data` field is missing"));
            }
            decode::<LoggingMessageParams>(&params)?;
        }
        ParamsValidator::ResourceUpdated => {
            if !params.get("uri").is_some_and(Value::is_string) {
                return Err(ParamsFailure::invalid(
                    "required `uri` field must be a string",
                ));
            }
        }
        ParamsValidator::TaskStatus => decode::<TaskStatusParams>(&params)?,
        ParamsValidator::ElicitationComplete => decode::<ElicitationCompleteParams>(&params)?,
        ParamsValidator::SubscriptionsAcknowledged => {
            decode::<SubscriptionsAcknowledgedParams>(&params)?;
        }
    }

    if revision == McpProtocolRevision::V2026_07_28 && rule.kind == McpCallKind::Request {
        validate_2026_request_meta(&params)?;
    }

    Ok(())
}

fn validate_2026_request_meta(params: &Value) -> Result<(), ParamsFailure> {
    let meta = params
        .get("_meta")
        .ok_or_else(|| ParamsFailure::invalid("required `_meta` field is missing"))?;
    let meta = decode_value::<RequestMeta>(meta)?;
    meta.validate_for_version("2026-07-28")
        .map_err(|error| ParamsFailure::invalid(error.to_string()))?;
    if meta.protocol_version.as_deref() != Some("2026-07-28") {
        return Err(ParamsFailure::invalid(
            "`_meta[\"io.modelcontextprotocol/protocolVersion\"]` must equal `2026-07-28`",
        ));
    }
    Ok(())
}

fn decode<T>(params: &Value) -> Result<(), ParamsFailure>
where
    T: DeserializeOwned,
{
    decode_value::<T>(params).map(|_| ())
}

fn decode_value<T>(params: &Value) -> Result<T, ParamsFailure>
where
    T: DeserializeOwned,
{
    serde_json::from_value(params.clone())
        .map_err(|error| ParamsFailure::invalid(error.to_string()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn inspect(
        revision: &str,
        value: Value,
        direction: Option<McpDirection>,
    ) -> Result<McpInspection, McpInspectionError> {
        McpInspector::new(revision)?.inspect(&value, direction)
    }

    fn valid_2026_meta() -> Value {
        json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {}
        })
    }

    #[test]
    fn exact_profiles_include_standalone_2025_06_18() {
        assert_eq!(
            MCP_INSPECTION_PROFILES,
            &["2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26"]
        );
        for profile in MCP_INSPECTION_PROFILES {
            let inspector = McpInspector::new(profile).unwrap();
            assert_eq!(inspector.revision().as_str(), *profile);
        }

        let error = McpInspector::new("2099-01-01").unwrap_err();
        assert_eq!(error.kind(), McpInspectionErrorKind::UnsupportedProfile);
        assert_eq!(error.revision(), Some("2099-01-01"));
        assert!(error.detail().contains("2099-01-01"));
    }

    #[test]
    fn revision_parser_and_constructor_are_explicit() {
        assert_eq!(
            McpProtocolRevision::try_from("2025-06-18").unwrap(),
            McpProtocolRevision::V2025_06_18
        );
        assert_eq!(
            McpInspector::for_revision(McpProtocolRevision::V2025_03_26).revision(),
            McpProtocolRevision::V2025_03_26
        );
    }

    #[test]
    fn classifies_available_unavailable_and_extension_methods() {
        let available = inspect(
            "2025-11-25",
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            Some(McpDirection::ClientToServer),
        )
        .unwrap();
        assert_eq!(
            available.methods()[0].classification(),
            McpMethodClassification::Available
        );

        let unavailable = inspect(
            "2025-03-26",
            json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}),
            Some(McpDirection::ClientToServer),
        )
        .unwrap();
        assert_eq!(
            unavailable.methods()[0].classification(),
            McpMethodClassification::Unavailable
        );

        let extension = inspect(
            "2025-03-26",
            json!({
                "jsonrpc":"2.0","id":1,"method":"com.example/widgets",
                "params":{"x":1}
            }),
            Some(McpDirection::ClientToServer),
        )
        .unwrap();
        assert_eq!(
            extension.methods()[0].classification(),
            McpMethodClassification::Extension
        );
    }

    #[test]
    fn official_task_extension_method_is_known_but_not_in_core_profiles() {
        for revision in MCP_INSPECTION_PROFILES {
            let inspected = inspect(
                revision,
                json!({"jsonrpc":"2.0","id":1,"method":"tasks/update","params":{}}),
                None,
            )
            .unwrap();
            assert_eq!(
                inspected.methods()[0].classification(),
                McpMethodClassification::Unavailable
            );
        }
    }

    #[test]
    fn only_2025_03_26_accepts_top_level_batches() {
        let calls = json!([
            {"jsonrpc":"2.0","id":1,"method":"tools/list"},
            {"jsonrpc":"2.0","method":"notifications/initialized"}
        ]);
        let inspected = inspect(
            "2025-03-26",
            calls.clone(),
            Some(McpDirection::ClientToServer),
        )
        .unwrap();
        assert_eq!(inspected.methods().len(), 2);
        assert_eq!(inspected.methods()[0].batch_index(), Some(0));
        assert_eq!(inspected.methods()[1].batch_index(), Some(1));

        let responses = json!([
            {"jsonrpc":"2.0","id":1,"result":{}},
            {"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"missing"}}
        ]);
        assert!(inspect("2025-03-26", responses, None).is_ok());

        for revision in ["2025-06-18", "2025-11-25", "2026-07-28"] {
            let error = inspect(revision, calls.clone(), None).unwrap_err();
            assert_eq!(error.kind(), McpInspectionErrorKind::BatchUnavailable);
            assert_eq!(error.revision(), Some(revision));
        }
    }

    #[test]
    fn initialize_is_not_batchable() {
        let error = inspect(
            "2025-03-26",
            json!([{
                "jsonrpc":"2.0","id":1,"method":"initialize","params":{
                    "protocolVersion":"2025-03-26",
                    "capabilities":{},
                    "clientInfo":{"name":"test","version":"1"}
                }
            }]),
            Some(McpDirection::ClientToServer),
        )
        .unwrap_err();
        assert_eq!(error.kind(), McpInspectionErrorKind::InitializeInBatch);
        assert_eq!(error.method(), Some("initialize"));
        assert_eq!(error.batch_index(), Some(0));
    }

    #[test]
    fn direction_is_optional_but_enforced_when_present() {
        let sampling = json!({
            "jsonrpc":"2.0","id":1,"method":"sampling/createMessage","params":{
                "messages":[], "maxTokens":10
            }
        });
        assert!(inspect("2025-11-25", sampling.clone(), None).is_ok());
        assert!(
            inspect(
                "2025-11-25",
                sampling.clone(),
                Some(McpDirection::ServerToClient)
            )
            .is_ok()
        );
        let error =
            inspect("2025-11-25", sampling, Some(McpDirection::ClientToServer)).unwrap_err();
        assert_eq!(error.kind(), McpInspectionErrorKind::DirectionMismatch);
    }

    #[test]
    fn method_kind_is_checked_for_available_methods() {
        let error = inspect(
            "2025-11-25",
            json!({"jsonrpc":"2.0","method":"tools/list"}),
            Some(McpDirection::ClientToServer),
        )
        .unwrap_err();
        assert_eq!(error.kind(), McpInspectionErrorKind::MessageKindMismatch);
        assert_eq!(error.method(), Some("tools/list"));
    }

    #[test]
    fn missing_and_malformed_present_params_are_distinct() {
        let missing = inspect(
            "2025-11-25",
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call"}),
            None,
        )
        .unwrap_err();
        assert_eq!(missing.kind(), McpInspectionErrorKind::MissingParams);

        let malformed = inspect(
            "2025-11-25",
            json!({
                "jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":7}
            }),
            None,
        )
        .unwrap_err();
        assert_eq!(malformed.kind(), McpInspectionErrorKind::InvalidParams);

        assert!(
            inspect(
                "2025-11-25",
                json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn known_mcp_params_must_be_objects() {
        let error = inspect(
            "2025-11-25",
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":[]}),
            None,
        )
        .unwrap_err();
        assert_eq!(error.kind(), McpInspectionErrorKind::InvalidParams);
    }

    #[test]
    fn standalone_2025_06_18_profile_has_its_own_method_shape() {
        let form = json!({
            "jsonrpc":"2.0","id":1,"method":"elicitation/create","params":{
                "message":"Name?",
                "requestedSchema":{"type":"object","properties":{}}
            }
        });
        assert!(inspect("2025-06-18", form, Some(McpDirection::ServerToClient)).is_ok());

        let url = json!({
            "jsonrpc":"2.0","id":1,"method":"elicitation/create","params":{
                "mode":"url",
                "elicitationId":"id",
                "message":"Sign in",
                "url":"https://example.com"
            }
        });
        assert!(
            inspect(
                "2025-11-25",
                url.clone(),
                Some(McpDirection::ServerToClient)
            )
            .is_ok()
        );
        let error = inspect("2025-06-18", url, Some(McpDirection::ServerToClient)).unwrap_err();
        assert_eq!(error.kind(), McpInspectionErrorKind::InvalidParams);
    }

    #[test]
    fn current_profile_requires_exact_per_request_metadata() {
        let request = |meta: Value| {
            json!({
                "jsonrpc":"2.0","id":1,"method":"tools/list",
                "params":{"_meta":meta}
            })
        };
        let inspected = inspect(
            "2026-07-28",
            request(valid_2026_meta()),
            Some(McpDirection::ClientToServer),
        )
        .unwrap();
        assert_eq!(inspected.revision(), McpProtocolRevision::V2026_07_28);
        assert_eq!(inspected.direction(), Some(McpDirection::ClientToServer));
        assert!(inspected.payload().as_single().is_some());

        for meta in [
            json!({}),
            json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28"}),
            json!({
                "io.modelcontextprotocol/protocolVersion":"2025-11-25",
                "io.modelcontextprotocol/clientCapabilities":{}
            }),
        ] {
            let error = inspect("2026-07-28", request(meta), None).unwrap_err();
            assert_eq!(error.kind(), McpInspectionErrorKind::InvalidParams);
        }
    }

    #[test]
    fn current_profile_requires_params_even_for_list_methods() {
        let error = inspect(
            "2026-07-28",
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            None,
        )
        .unwrap_err();
        assert_eq!(error.kind(), McpInspectionErrorKind::MissingParams);
    }

    #[test]
    fn current_profile_rejects_removed_top_level_methods() {
        for method in [
            "initialize",
            "ping",
            "sampling/createMessage",
            "roots/list",
            "elicitation/create",
            "resources/subscribe",
        ] {
            let inspected = inspect(
                "2026-07-28",
                json!({"jsonrpc":"2.0","id":1,"method":method,"params":{}}),
                None,
            )
            .unwrap();
            assert_eq!(
                inspected.methods()[0].classification(),
                McpMethodClassification::Unavailable,
                "{method}"
            );
        }
    }

    #[test]
    fn current_notification_direction_rules_are_stateless() {
        let cancelled = json!({
            "jsonrpc":"2.0","method":"notifications/cancelled",
            "params":{"requestId":1}
        });
        for direction in [McpDirection::ClientToServer, McpDirection::ServerToClient] {
            assert!(inspect("2026-07-28", cancelled.clone(), Some(direction)).is_ok());
        }

        let progress = json!({
            "jsonrpc":"2.0","method":"notifications/progress",
            "params":{"progressToken":"x","progress":1}
        });
        let error =
            inspect("2026-07-28", progress, Some(McpDirection::ClientToServer)).unwrap_err();
        assert_eq!(error.kind(), McpInspectionErrorKind::DirectionMismatch);
    }

    #[test]
    fn notification_params_get_typed_validation() {
        let missing_cancel_id = inspect(
            "2025-11-25",
            json!({
                "jsonrpc":"2.0","method":"notifications/cancelled","params":{}
            }),
            None,
        )
        .unwrap_err();
        assert_eq!(
            missing_cancel_id.kind(),
            McpInspectionErrorKind::InvalidParams
        );

        let missing_log_data = inspect(
            "2025-11-25",
            json!({
                "jsonrpc":"2.0","method":"notifications/message",
                "params":{"level":"info"}
            }),
            Some(McpDirection::ServerToClient),
        )
        .unwrap_err();
        assert_eq!(
            missing_log_data.kind(),
            McpInspectionErrorKind::InvalidParams
        );

        assert!(
            inspect(
                "2025-11-25",
                json!({
                    "jsonrpc":"2.0","method":"notifications/resources/updated",
                    "params":{"uri":"file:///tmp/test"}
                }),
                Some(McpDirection::ServerToClient)
            )
            .is_ok()
        );
    }

    #[test]
    fn response_inspection_does_not_claim_correlation() {
        let inspected = inspect(
            "2026-07-28",
            json!({"jsonrpc":"2.0","id":1,"result":{"resultType":"complete"}}),
            Some(McpDirection::ServerToClient),
        )
        .unwrap();
        assert!(inspected.methods().is_empty());
        assert!(matches!(
            inspected.into_payload().as_single(),
            Some(JsonRpcEnvelope::Result(_))
        ));
    }

    #[test]
    fn structural_errors_are_preserved() {
        let error = inspect(
            "2025-11-25",
            json!({"jsonrpc":"1.0","id":1,"method":"tools/list"}),
            None,
        )
        .unwrap_err();
        assert_eq!(error.kind(), McpInspectionErrorKind::JsonRpc);
        assert!(error.json_rpc_error().is_some());
        assert_eq!(error.revision(), Some("2025-11-25"));
    }

    #[test]
    fn method_table_has_unique_names_and_expected_size() {
        let mut names = METHOD_RULES
            .iter()
            .map(|rule| rule.name)
            .collect::<Vec<_>>();
        let original_len = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), original_len);
        assert_eq!(original_len, 35);
    }

    #[test]
    fn method_matrix_matches_official_role_aggregates() {
        fn available(
            revision: McpProtocolRevision,
            kind: McpCallKind,
            direction: McpDirection,
        ) -> Vec<&'static str> {
            let mut methods = METHOD_RULES
                .iter()
                .filter(|rule| rule.kind == kind && rule.allows(revision, direction))
                .map(|rule| rule.name)
                .collect::<Vec<_>>();
            methods.sort_unstable();
            methods
        }

        fn expected(mut methods: Vec<&'static str>) -> Vec<&'static str> {
            methods.sort_unstable();
            methods
        }

        let legacy_client_requests = expected(vec![
            "completion/complete",
            "initialize",
            "logging/setLevel",
            "ping",
            "prompts/get",
            "prompts/list",
            "resources/list",
            "resources/read",
            "resources/subscribe",
            "resources/templates/list",
            "resources/unsubscribe",
            "tools/call",
            "tools/list",
        ]);
        let legacy_client_notifications = expected(vec![
            "notifications/cancelled",
            "notifications/initialized",
            "notifications/progress",
            "notifications/roots/list_changed",
        ]);
        let legacy_server_notifications = expected(vec![
            "notifications/cancelled",
            "notifications/message",
            "notifications/progress",
            "notifications/prompts/list_changed",
            "notifications/resources/list_changed",
            "notifications/resources/updated",
            "notifications/tools/list_changed",
        ]);

        for revision in [
            McpProtocolRevision::V2025_03_26,
            McpProtocolRevision::V2025_06_18,
        ] {
            assert_eq!(
                available(revision, McpCallKind::Request, McpDirection::ClientToServer),
                legacy_client_requests
            );
            assert_eq!(
                available(
                    revision,
                    McpCallKind::Notification,
                    McpDirection::ClientToServer
                ),
                legacy_client_notifications
            );
            assert_eq!(
                available(
                    revision,
                    McpCallKind::Notification,
                    McpDirection::ServerToClient
                ),
                legacy_server_notifications
            );
        }
        assert_eq!(
            available(
                McpProtocolRevision::V2025_03_26,
                McpCallKind::Request,
                McpDirection::ServerToClient
            ),
            expected(vec!["ping", "roots/list", "sampling/createMessage"])
        );
        assert_eq!(
            available(
                McpProtocolRevision::V2025_06_18,
                McpCallKind::Request,
                McpDirection::ServerToClient
            ),
            expected(vec![
                "elicitation/create",
                "ping",
                "roots/list",
                "sampling/createMessage"
            ])
        );

        let mut november_client_requests = legacy_client_requests.clone();
        november_client_requests.extend([
            "tasks/cancel",
            "tasks/get",
            "tasks/list",
            "tasks/result",
        ]);
        november_client_requests.sort_unstable();
        assert_eq!(
            available(
                McpProtocolRevision::V2025_11_25,
                McpCallKind::Request,
                McpDirection::ClientToServer
            ),
            november_client_requests
        );
        let mut november_client_notifications = legacy_client_notifications.clone();
        november_client_notifications.push("notifications/tasks/status");
        november_client_notifications.sort_unstable();
        assert_eq!(
            available(
                McpProtocolRevision::V2025_11_25,
                McpCallKind::Notification,
                McpDirection::ClientToServer
            ),
            november_client_notifications
        );
        assert_eq!(
            available(
                McpProtocolRevision::V2025_11_25,
                McpCallKind::Request,
                McpDirection::ServerToClient
            ),
            expected(vec![
                "elicitation/create",
                "ping",
                "roots/list",
                "sampling/createMessage",
                "tasks/cancel",
                "tasks/get",
                "tasks/list",
                "tasks/result"
            ])
        );
        let mut november_server_notifications = legacy_server_notifications.clone();
        november_server_notifications.extend([
            "notifications/elicitation/complete",
            "notifications/tasks/status",
        ]);
        november_server_notifications.sort_unstable();
        assert_eq!(
            available(
                McpProtocolRevision::V2025_11_25,
                McpCallKind::Notification,
                McpDirection::ServerToClient
            ),
            november_server_notifications
        );

        assert_eq!(
            available(
                McpProtocolRevision::V2026_07_28,
                McpCallKind::Request,
                McpDirection::ClientToServer
            ),
            expected(vec![
                "completion/complete",
                "prompts/get",
                "prompts/list",
                "resources/list",
                "resources/read",
                "resources/templates/list",
                "server/discover",
                "subscriptions/listen",
                "tools/call",
                "tools/list"
            ])
        );
        assert_eq!(
            available(
                McpProtocolRevision::V2026_07_28,
                McpCallKind::Notification,
                McpDirection::ClientToServer
            ),
            vec!["notifications/cancelled"]
        );
        assert!(
            available(
                McpProtocolRevision::V2026_07_28,
                McpCallKind::Request,
                McpDirection::ServerToClient
            )
            .is_empty()
        );
        assert_eq!(
            available(
                McpProtocolRevision::V2026_07_28,
                McpCallKind::Notification,
                McpDirection::ServerToClient
            ),
            expected(vec![
                "notifications/cancelled",
                "notifications/message",
                "notifications/progress",
                "notifications/prompts/list_changed",
                "notifications/resources/list_changed",
                "notifications/resources/updated",
                "notifications/subscriptions/acknowledged",
                "notifications/tools/list_changed"
            ])
        );
    }
}
