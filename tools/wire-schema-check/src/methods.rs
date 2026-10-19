//! Method name to schema definition table.
//!
//! The published schema does not say "the result of `tools/list` is
//! `ListToolsResult`", so the mapping is spelled out here. A method is looked
//! up by name only; whether the definitions exist in a given protocol version
//! is decided against that version's schema, so a row can serve every version
//! that defines its definitions and is simply "unknown" in the others (for
//! example `tasks/get` in `2025-06-18`, or `ping` in `2026-07-28`).
//!
//! To cover a new method, add one row. A method that is missing from the table
//! is counted in the report, never treated as a failure.

/// Definition names for one request method.
pub struct RequestDefs {
    pub method: &'static str,
    /// Definition of the whole request frame.
    pub request: &'static str,
    /// Definition of the `result` object of the matching response.
    pub result: &'static str,
}

/// Request methods, both directions.
pub const REQUESTS: &[RequestDefs] = &[
    // Lifecycle.
    RequestDefs {
        method: "initialize",
        request: "InitializeRequest",
        result: "InitializeResult",
    },
    RequestDefs {
        method: "ping",
        request: "PingRequest",
        result: "EmptyResult",
    },
    // 2026-07-28 stateless discovery and subscriptions.
    RequestDefs {
        method: "server/discover",
        request: "DiscoverRequest",
        result: "DiscoverResult",
    },
    RequestDefs {
        method: "subscriptions/listen",
        request: "SubscriptionsListenRequest",
        result: "SubscriptionsListenResult",
    },
    // Tools.
    RequestDefs {
        method: "tools/list",
        request: "ListToolsRequest",
        result: "ListToolsResult",
    },
    RequestDefs {
        method: "tools/call",
        request: "CallToolRequest",
        result: "CallToolResult",
    },
    // Resources.
    RequestDefs {
        method: "resources/list",
        request: "ListResourcesRequest",
        result: "ListResourcesResult",
    },
    RequestDefs {
        method: "resources/templates/list",
        request: "ListResourceTemplatesRequest",
        result: "ListResourceTemplatesResult",
    },
    RequestDefs {
        method: "resources/read",
        request: "ReadResourceRequest",
        result: "ReadResourceResult",
    },
    RequestDefs {
        method: "resources/subscribe",
        request: "SubscribeRequest",
        result: "EmptyResult",
    },
    RequestDefs {
        method: "resources/unsubscribe",
        request: "UnsubscribeRequest",
        result: "EmptyResult",
    },
    // Prompts.
    RequestDefs {
        method: "prompts/list",
        request: "ListPromptsRequest",
        result: "ListPromptsResult",
    },
    RequestDefs {
        method: "prompts/get",
        request: "GetPromptRequest",
        result: "GetPromptResult",
    },
    // Completion and logging.
    RequestDefs {
        method: "completion/complete",
        request: "CompleteRequest",
        result: "CompleteResult",
    },
    RequestDefs {
        method: "logging/setLevel",
        request: "SetLevelRequest",
        result: "EmptyResult",
    },
    // Tasks (2025-11-25 core; an extension in 2026-07-28, so no definitions there).
    RequestDefs {
        method: "tasks/get",
        request: "GetTaskRequest",
        result: "GetTaskResult",
    },
    RequestDefs {
        method: "tasks/result",
        request: "GetTaskPayloadRequest",
        result: "GetTaskPayloadResult",
    },
    RequestDefs {
        method: "tasks/cancel",
        request: "CancelTaskRequest",
        result: "CancelTaskResult",
    },
    RequestDefs {
        method: "tasks/list",
        request: "ListTasksRequest",
        result: "ListTasksResult",
    },
    // Server to client requests.
    RequestDefs {
        method: "sampling/createMessage",
        request: "CreateMessageRequest",
        result: "CreateMessageResult",
    },
    RequestDefs {
        method: "elicitation/create",
        request: "ElicitRequest",
        result: "ElicitResult",
    },
    RequestDefs {
        method: "roots/list",
        request: "ListRootsRequest",
        result: "ListRootsResult",
    },
];

/// Notification methods and the definition of the whole notification frame.
pub const NOTIFICATIONS: &[(&str, &str)] = &[
    ("notifications/initialized", "InitializedNotification"),
    ("notifications/cancelled", "CancelledNotification"),
    ("notifications/progress", "ProgressNotification"),
    ("notifications/message", "LoggingMessageNotification"),
    (
        "notifications/resources/list_changed",
        "ResourceListChangedNotification",
    ),
    (
        "notifications/resources/updated",
        "ResourceUpdatedNotification",
    ),
    (
        "notifications/tools/list_changed",
        "ToolListChangedNotification",
    ),
    (
        "notifications/prompts/list_changed",
        "PromptListChangedNotification",
    ),
    (
        "notifications/roots/list_changed",
        "RootsListChangedNotification",
    ),
    ("notifications/tasks/status", "TaskStatusNotification"),
    (
        "notifications/elicitation/complete",
        "ElicitationCompleteNotification",
    ),
    (
        "notifications/subscriptions/acknowledged",
        "SubscriptionsAcknowledgedNotification",
    ),
];

/// Look up the definitions for a request method.
pub fn request_defs(method: &str) -> Option<&'static RequestDefs> {
    REQUESTS.iter().find(|r| r.method == method)
}

/// Look up the definition for a notification method.
pub fn notification_def(method: &str) -> Option<&'static str> {
    NOTIFICATIONS
        .iter()
        .find(|(m, _)| *m == method)
        .map(|(_, def)| *def)
}

/// Definition candidates for a JSON-RPC error response, in preference order
/// (the name changed between 2025-06-18 and 2025-11-25).
pub const ERROR_RESPONSE_DEFS: &[&str] = &["JSONRPCErrorResponse", "JSONRPCError"];

/// Definition candidates for the envelope of a successful response.
pub const RESULT_RESPONSE_DEFS: &[&str] = &["JSONRPCResultResponse", "JSONRPCResponse"];
