//! What the router discloses to a client when something fails internally.
//!
//! Two policies live here, and they answer the same question for different
//! failures: [`PanicPolicy`] for a caught handler panic and a transport's own
//! internal error, [`TaskErrorPolicy`] for the Task lifecycle. Both default to
//! saying as little as possible, because the text they would otherwise copy is
//! written by a handler or a storage backend and can name hosts, paths, or
//! queries.

use super::*;

/// Recover a readable message from a panic payload.
///
/// `panic!` with a literal yields `&str` and with a format yields `String`;
/// anything else is opaque and reported as such rather than guessed at.
pub(super) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&'static str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "panicked with a non-string payload".to_string()
    }
}

#[derive(Clone)]
pub(super) enum ClientPanicMessage {
    Detailed,
    Fixed(Arc<str>),
}

#[derive(Clone)]
pub(super) enum ToolNameDisclosure {
    Omit,
    Original,
    Fixed(Arc<str>),
}

impl ToolNameDisclosure {
    pub(super) fn value<'a>(&'a self, original: &'a str) -> Option<&'a str> {
        match self {
            Self::Omit => None,
            Self::Original => Some(original),
            Self::Fixed(name) => Some(name),
        }
    }

    pub(super) fn mode(&self) -> &'static str {
        match self {
            Self::Omit => "omitted",
            Self::Original => "original",
            Self::Fixed(_) => "fixed",
        }
    }
}

/// Controls what Tower discloses after isolating a panicking tool handler.
///
/// Construct a redacted policy with [`PanicPolicy::redacted`], then opt in to
/// individual disclosures only when they are safe for the application. Panic
/// payloads are never included in a custom policy's client response.
///
/// Rust's process-global panic hook runs before Tower catches an unwind. This
/// policy governs only Tower's client response and Tower-generated tracing
/// event; it cannot redact output produced by an application-installed panic
/// hook or by Rust's default panic hook.
#[derive(Clone)]
pub struct PanicPolicy {
    pub(super) client_message: ClientPanicMessage,
    pub(super) client_tool_name: ToolNameDisclosure,
    pub(super) log_tool_name: ToolNameDisclosure,
    pub(super) include_payload_in_logs: bool,
}

impl std::fmt::Debug for PanicPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let client_message = match self.client_message {
            ClientPanicMessage::Detailed => "detailed",
            ClientPanicMessage::Fixed(_) => "fixed",
        };
        f.debug_struct("PanicPolicy")
            .field("client_message", &client_message)
            .field("client_tool_name", &self.client_tool_name.mode())
            .field("log_tool_name", &self.log_tool_name.mode())
            .field("include_payload_in_logs", &self.include_payload_in_logs)
            .finish()
    }
}

impl PanicPolicy {
    /// Create a policy whose client response is fixed application-supplied
    /// text and whose Tower tracing event contains neither the tool name nor
    /// the panic payload.
    pub fn redacted(client_message: impl Into<String>) -> Self {
        Self {
            client_message: ClientPanicMessage::Fixed(Arc::from(client_message.into())),
            client_tool_name: ToolNameDisclosure::Omit,
            log_tool_name: ToolNameDisclosure::Omit,
            include_payload_in_logs: false,
        }
    }

    pub(super) fn detailed() -> Self {
        Self {
            client_message: ClientPanicMessage::Detailed,
            client_tool_name: ToolNameDisclosure::Original,
            log_tool_name: ToolNameDisclosure::Original,
            include_payload_in_logs: true,
        }
    }

    /// Include the registered tool name in the client-visible error.
    ///
    /// With a redacted policy this changes the response from the exact fixed
    /// message to `tool '<name>': <fixed message>`.
    #[must_use]
    pub fn include_tool_name_in_client_message(mut self, include: bool) -> Self {
        self.client_tool_name = if include {
            ToolNameDisclosure::Original
        } else {
            ToolNameDisclosure::Omit
        };
        self
    }

    /// Replace the registered tool name in the client-visible error with a
    /// fixed application-selected label.
    ///
    /// This is useful when the original catalog name is sensitive but a
    /// stable category such as `provider tool` is still useful to callers.
    #[must_use]
    pub fn client_tool_name(mut self, name: impl Into<String>) -> Self {
        self.client_tool_name = ToolNameDisclosure::Fixed(Arc::from(name.into()));
        self
    }

    /// Include the registered tool name in Tower's panic tracing event.
    #[must_use]
    pub fn include_tool_name_in_logs(mut self, include: bool) -> Self {
        self.log_tool_name = if include {
            ToolNameDisclosure::Original
        } else {
            ToolNameDisclosure::Omit
        };
        self
    }

    /// Replace the registered tool name in Tower's panic tracing event with
    /// a fixed application-selected label.
    #[must_use]
    pub fn log_tool_name(mut self, name: impl Into<String>) -> Self {
        self.log_tool_name = ToolNameDisclosure::Fixed(Arc::from(name.into()));
        self
    }

    /// Include the recovered panic payload in Tower's panic tracing event.
    ///
    /// This switch never changes the client-visible error.
    #[must_use]
    pub fn include_payload_in_logs(mut self, include: bool) -> Self {
        self.include_payload_in_logs = include;
        self
    }

    pub(super) fn client_message(&self, tool_name: &str, payload: Option<&str>) -> String {
        match &self.client_message {
            ClientPanicMessage::Detailed => format!(
                "tool '{tool_name}' panicked: {}",
                payload.unwrap_or("<redacted>")
            ),
            ClientPanicMessage::Fixed(message) => match self.client_tool_name.value(tool_name) {
                Some(name) => format!("tool '{name}': {message}"),
                None => message.to_string(),
            },
        }
    }

    pub(super) fn needs_payload(&self) -> bool {
        matches!(self.client_message, ClientPanicMessage::Detailed) || self.include_payload_in_logs
    }

    /// The client-visible text for an internal failure that is not a caught
    /// tool panic.
    ///
    /// A transport that builds its own error response has no tool to name and
    /// no panic payload to redact, so it cannot use `client_message`. The
    /// operator's disclosure choice still applies: a policy installed to keep
    /// internal text away from clients should not be bypassed because the
    /// failure happened while framing a response rather than inside a handler.
    ///
    /// The tool-name switches are deliberately not consulted. They select how
    /// to name a tool, and there is no tool here to name.
    ///
    /// Gated with its caller. Widen the gate when a second transport adopts
    /// [`McpRouter::transport_internal_error`].
    #[cfg(feature = "websocket")]
    pub(super) fn internal_error_message(&self, error: &dyn std::fmt::Display) -> String {
        match &self.client_message {
            ClientPanicMessage::Detailed => error.to_string(),
            ClientPanicMessage::Fixed(message) => message.to_string(),
        }
    }
}

/// The Task operation whose failure is being exposed to a client.
///
/// A [`TaskErrorPolicy`] receives this alongside the typed failure so an
/// application can attach stable operation-specific data without parsing an
/// error message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TaskOperation {
    /// Creating and preparing a Task from a task-augmented request.
    Create,
    /// Reading a Task through `tasks/get`.
    Get,
    /// Applying client input through `tasks/update`.
    Update,
    /// Requesting cancellation through `tasks/cancel`.
    Cancel,
    /// Persisting an input-required transition requested by a Task handler.
    ParkInput,
    /// Task-store work performed inside a live Task handler after creation.
    Execute,
    /// Reading durable state needed to resume a replayed Task handler.
    Resume,
    /// Persisting a terminal Task outcome after handler execution.
    Finalize,
}

/// The typed reason a Task operation failed.
///
/// Store errors retain their original typed value for an explicitly installed
/// [`TaskErrorPolicy`]. Their display text may contain backend paths, queries,
/// or codec details and must not be copied into a client response without an
/// application-specific disclosure review. Tower's default policy never does
/// so.
#[non_exhaustive]
pub enum TaskFailure {
    /// The Task ID is unknown or its retained tombstone was removed.
    NotFound,
    /// The Task is known to have expired and the caller owns it.
    Expired,
    /// The Task store returned an error.
    Store(TaskStoreError),
    /// Tower detected a safe, static Task-lifecycle invariant failure.
    Internal(&'static str),
    /// Client-supplied Task arguments were malformed.
    InvalidArguments(&'static str),
    /// A live Task handler returned an unclassified execution error.
    ///
    /// The underlying error is deliberately neither logged nor exposed to the
    /// policy: its display text is application-owned and can contain provider
    /// details.
    Handler,
}

impl std::fmt::Debug for TaskFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("NotFound"),
            Self::Expired => f.write_str("Expired"),
            Self::Store(error) => {
                let kind = match error {
                    TaskStoreError::Encode(_) => "Encode",
                    TaskStoreError::Decode(_) => "Decode",
                    TaskStoreError::Backend(_) => "Backend",
                    TaskStoreError::InvalidTransition(_) => "InvalidTransition",
                    TaskStoreError::RetentionLimitExceeded { .. } => "RetentionLimitExceeded",
                };
                write!(f, "Store({kind})")
            }
            Self::Internal(message) => f.debug_tuple("Internal").field(message).finish(),
            Self::InvalidArguments(message) => {
                f.debug_tuple("InvalidArguments").field(message).finish()
            }
            Self::Handler => f.write_str("Handler"),
        }
    }
}

/// Typed input to a [`TaskErrorPolicy`].
///
/// The fields are intentionally private so Tower can add context without
/// breaking policy implementations. Inspect them through the accessors.
#[derive(Debug)]
#[non_exhaustive]
pub struct TaskErrorContext {
    operation: TaskOperation,
    task_id: Option<String>,
    failure: TaskFailure,
}

impl TaskErrorContext {
    pub(super) fn new(
        operation: TaskOperation,
        task_id: Option<&str>,
        failure: TaskFailure,
    ) -> Self {
        Self {
            operation,
            task_id: task_id.map(str::to_owned),
            failure,
        }
    }

    /// The Task operation that failed.
    pub const fn operation(&self) -> TaskOperation {
        self.operation
    }

    /// The Task ID, when one had been allocated or supplied.
    pub fn task_id(&self) -> Option<&str> {
        self.task_id.as_deref()
    }

    /// The typed failure.
    pub const fn failure(&self) -> &TaskFailure {
        &self.failure
    }
}

type TaskErrorMapper = dyn Fn(&TaskErrorContext) -> JsonRpcError + Send + Sync + 'static;

/// Maps Task lifecycle failures to client-visible JSON-RPC errors.
///
/// The default preserves Tower's established `-32602` shapes for unknown and
/// expired Tasks. Store failures use `-32603` with fixed text, deliberately
/// omitting the store error's display string because it may disclose backend
/// details.
///
/// A custom policy can attach an application's structured error envelope. It
/// receives the original [`TaskStoreError`] by reference, so it must make its
/// own explicit disclosure decision rather than forwarding `Display` text.
#[derive(Clone)]
pub struct TaskErrorPolicy {
    mapper: Arc<TaskErrorMapper>,
}

impl std::fmt::Debug for TaskErrorPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskErrorPolicy").finish_non_exhaustive()
    }
}

impl Default for TaskErrorPolicy {
    fn default() -> Self {
        Self::new(default_task_error)
    }
}

impl TaskErrorPolicy {
    /// Construct a Task error policy from a synchronous mapper.
    ///
    /// The mapper runs only after Tower has authorized any distinction between
    /// an expired Task and a missing one. An unauthorized Task is passed to the
    /// mapper as [`TaskFailure::NotFound`], exactly like a never-issued ID. It
    /// runs synchronously on the request or handler path, so it should be fast.
    /// Tower catches a panic and substitutes a fixed redacted internal error.
    /// Rust's process-global panic hook still runs before the unwind is caught.
    pub fn new<F>(mapper: F) -> Self
    where
        F: Fn(&TaskErrorContext) -> JsonRpcError + Send + Sync + 'static,
    {
        Self {
            mapper: Arc::new(mapper),
        }
    }

    pub(super) fn map(&self, context: &TaskErrorContext) -> JsonRpcError {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.mapper)(context))) {
            Ok(error) => error,
            Err(_) => {
                // The policy itself is application code. Record that it broke,
                // but do not attach the Task ID, failure, or panic payload:
                // this is the final redaction boundary.
                tracing::error!(
                    target: "mcp::tasks",
                    "task error policy panicked; using the redacted fallback"
                );
                JsonRpcError::internal_error("Task error policy failed")
            }
        }
    }

    pub(crate) fn map_store_error(
        &self,
        operation: TaskOperation,
        task_id: &str,
        error: TaskStoreError,
    ) -> JsonRpcError {
        self.map(&TaskErrorContext::new(
            operation,
            Some(task_id),
            TaskFailure::Store(error),
        ))
    }

    pub(crate) fn map_internal_error(
        &self,
        operation: TaskOperation,
        task_id: &str,
        message: &'static str,
    ) -> JsonRpcError {
        self.map(&TaskErrorContext::new(
            operation,
            Some(task_id),
            TaskFailure::Internal(message),
        ))
    }
}

pub(super) fn default_task_error(context: &TaskErrorContext) -> JsonRpcError {
    match context.failure() {
        TaskFailure::NotFound => JsonRpcError::invalid_params(format!(
            "Task not found: {}",
            context.task_id().unwrap_or("<unknown>")
        )),
        TaskFailure::Expired => JsonRpcError::invalid_params(format!(
            "Task expired: {}",
            context.task_id().unwrap_or("<unknown>")
        ))
        .with_data(serde_json::json!({ "reason": "task_expired" })),
        TaskFailure::Store(_) => JsonRpcError::internal_error("Task store operation failed"),
        TaskFailure::Internal(message) => JsonRpcError::internal_error(*message),
        TaskFailure::InvalidArguments(message) => JsonRpcError::invalid_params(*message),
        TaskFailure::Handler => JsonRpcError::internal_error("Task handler failed"),
    }
}
