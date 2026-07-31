//! Exact wire types for the final MCP Tasks extension (SEP-2663).
//!
//! These types deliberately live outside [`crate::protocol`]'s legacy task
//! types. The 2025-11-25 experimental Tasks feature is not wire-compatible
//! with the final `io.modelcontextprotocol/tasks` extension:
//!
//! - final task timestamps use `ttlMs` and `pollIntervalMs`;
//! - final task creation is a flat `resultType: "task"` result;
//! - `tasks/get` returns a status-discriminated [`DetailedTask`];
//! - `tasks/update` and `tasks/cancel` return complete acknowledgements.
//!
//! Keeping both surfaces distinct lets a runtime choose the correct wire model
//! from the negotiated protocol version without emitting mixed compatibility
//! objects.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::error::JsonRpcError;
use crate::protocol::{InputRequests, InputResponses, RequestMeta, ResultType, TaskStatus};

/// Extension identifier for final Tasks capability negotiation.
pub const EXTENSION_ID: &str = crate::protocol::TASKS_EXTENSION_ID;

/// Fields shared by every final task state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskMetadata {
    /// Stable, server-generated task identifier.
    pub task_id: String,
    /// Optional human-readable state description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
    /// ISO 8601 timestamp of the latest state change.
    pub last_updated_at: String,
    /// Time-to-live from creation in milliseconds, or `null` for unlimited.
    ///
    /// Unlike an omitted optional field, SEP-2663 requires `ttlMs` to be
    /// present. `None` therefore serializes as JSON `null`.
    pub ttl_ms: Option<u64>,
    /// Suggested client polling interval in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval_ms: Option<u64>,
}

impl TaskMetadata {
    /// Construct the required final task metadata.
    pub fn new(
        task_id: impl Into<String>,
        created_at: impl Into<String>,
        last_updated_at: impl Into<String>,
        ttl_ms: Option<u64>,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            status_message: None,
            created_at: created_at.into(),
            last_updated_at: last_updated_at.into(),
            ttl_ms,
            poll_interval_ms: None,
        }
    }

    /// Set a human-readable status message.
    pub fn with_status_message(mut self, message: impl Into<String>) -> Self {
        self.status_message = Some(message.into());
        self
    }

    /// Set the suggested polling interval.
    pub fn with_poll_interval_ms(mut self, poll_interval_ms: u64) -> Self {
        self.poll_interval_ms = Some(poll_interval_ms);
        self
    }
}

/// Base task state embedded in a final [`CreateTaskResult`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// Shared task fields.
    #[serde(flatten)]
    pub metadata: TaskMetadata,
    /// Current task status.
    pub status: TaskStatus,
}

impl Task {
    /// Construct a base task state.
    pub fn new(metadata: TaskMetadata, status: TaskStatus) -> Self {
        Self { metadata, status }
    }
}

/// Complete status-specific task state returned by `tasks/get` and
/// `notifications/tasks`.
///
/// The internally tagged representation guarantees that required payloads
/// match their status: `input_required` has `inputRequests`, `completed` has
/// `result`, and `failed` has `error`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DetailedTask {
    /// Work is currently in progress.
    Working {
        /// Shared task fields.
        #[serde(flatten)]
        metadata: TaskMetadata,
    },
    /// Work is paused until the client fulfils the outstanding requests.
    InputRequired {
        /// Shared task fields.
        #[serde(flatten)]
        metadata: TaskMetadata,
        /// All currently outstanding server-to-client requests.
        #[serde(rename = "inputRequests")]
        input_requests: InputRequests,
    },
    /// Work completed successfully.
    ///
    /// Tool results with `isError: true` still use this variant.
    Completed {
        /// Shared task fields.
        #[serde(flatten)]
        metadata: TaskMetadata,
        /// The exact object the original synchronous request would return.
        result: Map<String, Value>,
    },
    /// Execution failed with a JSON-RPC error.
    Failed {
        /// Shared task fields.
        #[serde(flatten)]
        metadata: TaskMetadata,
        /// Structured JSON-RPC execution error.
        error: JsonRpcError,
    },
    /// Work was cancelled before completion.
    Cancelled {
        /// Shared task fields.
        #[serde(flatten)]
        metadata: TaskMetadata,
    },
}

impl DetailedTask {
    /// Construct a working task.
    pub fn working(metadata: TaskMetadata) -> Self {
        Self::Working { metadata }
    }

    /// Construct an input-required task.
    pub fn input_required(metadata: TaskMetadata, input_requests: InputRequests) -> Self {
        Self::InputRequired {
            metadata,
            input_requests,
        }
    }

    /// Construct a completed task from an object-valued synchronous result.
    pub fn completed(metadata: TaskMetadata, result: Map<String, Value>) -> Self {
        Self::Completed { metadata, result }
    }

    /// Construct a failed task from a structured JSON-RPC error.
    pub fn failed(metadata: TaskMetadata, error: JsonRpcError) -> Self {
        Self::Failed { metadata, error }
    }

    /// Construct a cancelled task.
    pub fn cancelled(metadata: TaskMetadata) -> Self {
        Self::Cancelled { metadata }
    }

    /// Current task status.
    pub fn status(&self) -> TaskStatus {
        match self {
            Self::Working { .. } => TaskStatus::Working,
            Self::InputRequired { .. } => TaskStatus::InputRequired,
            Self::Completed { .. } => TaskStatus::Completed,
            Self::Failed { .. } => TaskStatus::Failed,
            Self::Cancelled { .. } => TaskStatus::Cancelled,
        }
    }

    /// Server-generated identifier of the task this describes.
    pub fn task_id(&self) -> &str {
        &self.metadata().task_id
    }

    /// Shared task metadata.
    pub fn metadata(&self) -> &TaskMetadata {
        match self {
            Self::Working { metadata }
            | Self::InputRequired { metadata, .. }
            | Self::Completed { metadata, .. }
            | Self::Failed { metadata, .. }
            | Self::Cancelled { metadata } => metadata,
        }
    }

    /// Outstanding requests when the task is input-required.
    pub fn input_requests(&self) -> Option<&InputRequests> {
        match self {
            Self::InputRequired { input_requests, .. } => Some(input_requests),
            _ => None,
        }
    }

    /// Completed result object.
    pub fn result(&self) -> Option<&Map<String, Value>> {
        match self {
            Self::Completed { result, .. } => Some(result),
            _ => None,
        }
    }

    /// Failed JSON-RPC error.
    pub fn error(&self) -> Option<&JsonRpcError> {
        match self {
            Self::Failed { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Flat task handle returned instead of a synchronous result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTaskResult {
    #[serde(
        rename = "resultType",
        deserialize_with = "deserialize_task_result_type"
    )]
    result_type: ResultType,
    /// Seed task state. The fields are flattened on the wire.
    #[serde(flatten)]
    pub task: Task,
    /// Optional result metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
}

impl CreateTaskResult {
    /// Construct a flat final task result.
    pub fn new(task: Task) -> Self {
        Self {
            result_type: ResultType::Other(crate::protocol::RESULT_TYPE_TASK.to_string()),
            task,
            meta: None,
        }
    }

    /// The fixed `resultType` discriminator.
    pub fn result_type(&self) -> &ResultType {
        &self.result_type
    }
}

/// Complete result of `tasks/get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTaskResult {
    #[serde(
        rename = "resultType",
        deserialize_with = "deserialize_complete_result_type"
    )]
    result_type: ResultType,
    /// Status-specific task state, flattened on the wire.
    #[serde(flatten)]
    pub task: DetailedTask,
    /// Optional result metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
}

impl GetTaskResult {
    /// Construct a complete `tasks/get` result.
    pub fn new(task: DetailedTask) -> Self {
        Self {
            result_type: ResultType::Complete,
            task,
            meta: None,
        }
    }

    /// The fixed `resultType` discriminator.
    pub fn result_type(&self) -> &ResultType {
        &self.result_type
    }
}

/// Complete empty acknowledgement for `tasks/update` or `tasks/cancel`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskAcknowledgement {
    #[serde(
        rename = "resultType",
        deserialize_with = "deserialize_complete_result_type"
    )]
    result_type: ResultType,
    /// Optional result metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
}

impl TaskAcknowledgement {
    /// Construct a complete empty acknowledgement.
    pub fn new() -> Self {
        Self {
            result_type: ResultType::Complete,
            meta: None,
        }
    }

    /// The fixed `resultType` discriminator.
    pub fn result_type(&self) -> &ResultType {
        &self.result_type
    }
}

impl Default for TaskAcknowledgement {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of `tasks/update`.
pub type UpdateTaskResult = TaskAcknowledgement;

/// Result of `tasks/cancel`.
pub type CancelTaskResult = TaskAcknowledgement;

/// Parameters for `tasks/get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTaskParams {
    /// Task identifier to query.
    pub task_id: String,
    /// Required final-protocol request metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<RequestMeta>,
}

/// Parameters for `tasks/update`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTaskParams {
    /// Task identifier to update.
    pub task_id: String,
    /// Responses to currently outstanding task input requests.
    pub input_responses: InputResponses,
    /// Required final-protocol request metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<RequestMeta>,
}

/// Parameters for `tasks/cancel`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelTaskParams {
    /// Task identifier to cancel.
    pub task_id: String,
    /// Required final-protocol request metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<RequestMeta>,
}

/// Parameters for `notifications/tasks`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStatusNotificationParams {
    /// Complete status-specific task state.
    #[serde(flatten)]
    pub task: DetailedTask,
    /// Optional notification metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
}

fn deserialize_task_result_type<'de, D>(deserializer: D) -> Result<ResultType, D::Error>
where
    D: Deserializer<'de>,
{
    let result_type = ResultType::deserialize(deserializer)?;
    if result_type.as_str() == crate::protocol::RESULT_TYPE_TASK {
        Ok(result_type)
    } else {
        Err(serde::de::Error::custom(
            "resultType must be \"task\" for CreateTaskResult",
        ))
    }
}

fn deserialize_complete_result_type<'de, D>(deserializer: D) -> Result<ResultType, D::Error>
where
    D: Deserializer<'de>,
{
    let result_type = ResultType::deserialize(deserializer)?;
    if result_type == ResultType::Complete {
        Ok(result_type)
    } else {
        Err(serde::de::Error::custom("resultType must be \"complete\""))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::protocol::{
        ElicitAction, ElicitResult, InputRequest, InputResponse, ListRootsParams,
    };

    fn metadata() -> TaskMetadata {
        TaskMetadata::new(
            "task-2f68f7c9",
            "2026-07-30T00:00:00Z",
            "2026-07-30T00:00:01Z",
            Some(60_000),
        )
        .with_poll_interval_ms(2_000)
    }

    #[test]
    fn task_metadata_uses_exact_final_field_names_and_required_null_ttl() {
        let value = serde_json::to_value(Task::new(metadata(), TaskStatus::Working)).unwrap();
        assert_eq!(value["ttlMs"], 60_000);
        assert_eq!(value["pollIntervalMs"], 2_000);
        assert!(value.get("ttl").is_none());
        assert!(value.get("pollInterval").is_none());

        let unlimited = TaskMetadata::new(
            "task-unlimited",
            "2026-07-30T00:00:00Z",
            "2026-07-30T00:00:00Z",
            None,
        );
        let value = serde_json::to_value(Task::new(unlimited, TaskStatus::Working)).unwrap();
        assert_eq!(value["ttlMs"], Value::Null);
    }

    #[test]
    fn create_task_result_is_flat_and_rejects_wrong_discriminator() {
        let result = CreateTaskResult::new(Task::new(metadata(), TaskStatus::Working));
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["resultType"], "task");
        assert_eq!(value["taskId"], "task-2f68f7c9");
        assert!(value.get("task").is_none());

        let mut invalid = value;
        invalid["resultType"] = json!("complete");
        assert!(serde_json::from_value::<CreateTaskResult>(invalid).is_err());
    }

    #[test]
    fn detailed_task_variants_have_status_specific_payloads() {
        let working =
            serde_json::to_value(GetTaskResult::new(DetailedTask::working(metadata()))).unwrap();
        assert_eq!(working["resultType"], "complete");
        assert_eq!(working["status"], "working");
        assert!(working.get("inputRequests").is_none());
        assert!(working.get("result").is_none());
        assert!(working.get("error").is_none());

        let mut requests = InputRequests::new();
        requests.insert(
            "roots".to_string(),
            InputRequest::ListRoots(ListRootsParams { meta: None }),
        );
        let input_required = serde_json::to_value(GetTaskResult::new(
            DetailedTask::input_required(metadata(), requests),
        ))
        .unwrap();
        assert_eq!(input_required["status"], "input_required");
        assert_eq!(
            input_required["inputRequests"]["roots"]["method"],
            "roots/list"
        );

        let result = json!({
            "content": [{"type": "text", "text": "domain error"}],
            "isError": true
        })
        .as_object()
        .unwrap()
        .clone();
        let completed = serde_json::to_value(GetTaskResult::new(DetailedTask::completed(
            metadata(),
            result,
        )))
        .unwrap();
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["result"]["isError"], true);

        let failed = serde_json::to_value(GetTaskResult::new(DetailedTask::failed(
            metadata(),
            JsonRpcError::internal_error("worker crashed"),
        )))
        .unwrap();
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["error"]["code"], -32603);

        let cancelled =
            serde_json::to_value(GetTaskResult::new(DetailedTask::cancelled(metadata()))).unwrap();
        assert_eq!(cancelled["status"], "cancelled");
    }

    #[test]
    fn detailed_task_rejects_missing_required_status_payloads() {
        let base = json!({
            "taskId": "task-1",
            "createdAt": "2026-07-30T00:00:00Z",
            "lastUpdatedAt": "2026-07-30T00:00:00Z",
            "ttlMs": null
        });
        for status in ["input_required", "completed", "failed"] {
            let mut value = base.clone();
            value["status"] = json!(status);
            assert!(
                serde_json::from_value::<DetailedTask>(value).is_err(),
                "{status}"
            );
        }
    }

    #[test]
    fn update_params_are_typed_and_acknowledgements_are_complete() {
        let mut responses = InputResponses::new();
        responses.insert(
            "approval".to_string(),
            InputResponse::Elicit(ElicitResult {
                action: ElicitAction::Accept,
                content: None,
                meta: None,
            }),
        );
        let params = UpdateTaskParams {
            task_id: "task-2f68f7c9".to_string(),
            input_responses: responses,
            meta: None,
        };
        let value = serde_json::to_value(params).unwrap();
        assert_eq!(value["taskId"], "task-2f68f7c9");
        assert_eq!(
            value["inputResponses"]["approval"]["action"],
            json!("accept")
        );

        let ack = serde_json::to_value(TaskAcknowledgement::new()).unwrap();
        assert_eq!(ack, json!({"resultType": "complete"}));
        assert!(
            serde_json::from_value::<TaskAcknowledgement>(json!({
                "resultType": "task"
            }))
            .is_err()
        );
    }

    #[test]
    fn notification_payload_matches_tasks_get_detailed_shape() {
        let notification = TaskStatusNotificationParams {
            task: DetailedTask::cancelled(metadata()),
            meta: None,
        };
        let value = serde_json::to_value(notification).unwrap();
        assert_eq!(value["status"], "cancelled");
        assert_eq!(value["taskId"], "task-2f68f7c9");
        assert_eq!(value["ttlMs"], 60_000);
    }
}
