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

use serde::{Deserialize, Deserializer, Serialize, Serializer};
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
///
/// Each variant stores unknown task fields in its `additional` map. Use
/// `..` when matching a variant if you only need its typed fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DetailedTask {
    /// Work is currently in progress.
    Working {
        /// Shared task fields.
        #[serde(flatten)]
        metadata: TaskMetadata,
        /// Additional top-level task fields.
        #[serde(flatten, with = "detailed_additional")]
        additional: Map<String, Value>,
    },
    /// Work is paused until the client fulfils the outstanding requests.
    InputRequired {
        /// Shared task fields.
        #[serde(flatten)]
        metadata: TaskMetadata,
        /// All currently outstanding server-to-client requests.
        #[serde(rename = "inputRequests")]
        input_requests: InputRequests,
        /// Additional top-level task fields.
        #[serde(flatten, with = "detailed_additional")]
        additional: Map<String, Value>,
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
        /// Additional top-level task fields.
        #[serde(flatten, with = "detailed_additional")]
        additional: Map<String, Value>,
    },
    /// Execution failed with a JSON-RPC error.
    Failed {
        /// Shared task fields.
        #[serde(flatten)]
        metadata: TaskMetadata,
        /// Structured JSON-RPC execution error.
        error: JsonRpcError,
        /// Additional top-level task fields.
        #[serde(flatten, with = "detailed_additional")]
        additional: Map<String, Value>,
    },
    /// Work was cancelled before completion.
    Cancelled {
        /// Shared task fields.
        #[serde(flatten)]
        metadata: TaskMetadata,
        /// Additional top-level task fields.
        #[serde(flatten, with = "detailed_additional")]
        additional: Map<String, Value>,
    },
}

impl DetailedTask {
    /// Construct a working task.
    pub fn working(metadata: TaskMetadata) -> Self {
        Self::Working {
            metadata,
            additional: Map::new(),
        }
    }

    /// Construct an input-required task.
    pub fn input_required(metadata: TaskMetadata, input_requests: InputRequests) -> Self {
        Self::InputRequired {
            metadata,
            input_requests,
            additional: Map::new(),
        }
    }

    /// Construct a completed task from an object-valued synchronous result.
    pub fn completed(metadata: TaskMetadata, result: Map<String, Value>) -> Self {
        Self::Completed {
            metadata,
            result,
            additional: Map::new(),
        }
    }

    /// Construct a failed task from a structured JSON-RPC error.
    pub fn failed(metadata: TaskMetadata, error: JsonRpcError) -> Self {
        Self::Failed {
            metadata,
            error,
            additional: Map::new(),
        }
    }

    /// Construct a cancelled task.
    pub fn cancelled(metadata: TaskMetadata) -> Self {
        Self::Cancelled {
            metadata,
            additional: Map::new(),
        }
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
            Self::Working { metadata, .. }
            | Self::InputRequired { metadata, .. }
            | Self::Completed { metadata, .. }
            | Self::Failed { metadata, .. }
            | Self::Cancelled { metadata, .. } => metadata,
        }
    }

    /// Unknown top-level task fields.
    pub fn additional(&self) -> &Map<String, Value> {
        match self {
            Self::Working { additional, .. }
            | Self::InputRequired { additional, .. }
            | Self::Completed { additional, .. }
            | Self::Failed { additional, .. }
            | Self::Cancelled { additional, .. } => additional,
        }
    }

    /// Edit additional task fields. Serialization skips keys reserved by
    /// the task or its enclosing result or notification.
    pub fn additional_mut(&mut self) -> &mut Map<String, Value> {
        match self {
            Self::Working { additional, .. }
            | Self::InputRequired { additional, .. }
            | Self::Completed { additional, .. }
            | Self::Failed { additional, .. }
            | Self::Cancelled { additional, .. } => additional,
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
    /// Additional top-level fields. Serialization skips keys reserved by
    /// the task or this result.
    #[serde(flatten, with = "create_additional")]
    pub additional: Map<String, Value>,
}

impl CreateTaskResult {
    /// Construct a flat final task result.
    pub fn new(task: Task) -> Self {
        Self {
            result_type: ResultType::Other(crate::protocol::RESULT_TYPE_TASK.to_string()),
            task,
            meta: None,
            additional: Map::new(),
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
    /// Status-specific task state and additional fields, flattened on the wire.
    #[serde(flatten, serialize_with = "serialize_get_task")]
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
    /// Complete status-specific task state and additional fields.
    #[serde(flatten, serialize_with = "serialize_notification_task")]
    pub task: DetailedTask,
    /// Optional notification metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
}

fn is_task_field(key: &str) -> bool {
    matches!(
        key,
        "status"
            | "taskId"
            | "statusMessage"
            | "createdAt"
            | "lastUpdatedAt"
            | "ttlMs"
            | "pollIntervalMs"
    )
}

mod create_additional {
    use super::*;

    fn is_reserved(key: &str) -> bool {
        is_task_field(key) || matches!(key, "resultType" | "_meta")
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Map<String, Value>, D::Error> {
        // Task itself uses flatten, leaving its typed fields visible to this map.
        let mut additional = Map::deserialize(deserializer)?;
        additional.retain(|key, _| !is_reserved(key));
        Ok(additional)
    }

    pub(super) fn serialize<S: Serializer>(
        additional: &Map<String, Value>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_map(additional.iter().filter(|(key, _)| !is_reserved(key)))
    }
}

mod detailed_additional {
    use super::*;

    fn is_reserved(key: &str) -> bool {
        is_task_field(key) || matches!(key, "inputRequests" | "result" | "error")
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Map<String, Value>, D::Error> {
        let mut additional = Map::deserialize(deserializer)?;
        additional.retain(|key, _| !is_reserved(key));
        Ok(additional)
    }

    pub(super) fn serialize<S: Serializer>(
        additional: &Map<String, Value>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_map(additional.iter().filter(|(key, _)| !is_reserved(key)))
    }
}

fn serialize_get_task<S: Serializer>(
    task: &DetailedTask,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serialize_enclosed_task(task, &["resultType", "_meta"], serializer)
}

fn serialize_notification_task<S: Serializer>(
    task: &DetailedTask,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serialize_enclosed_task(task, &["_meta"], serializer)
}

fn serialize_enclosed_task<S: Serializer>(
    task: &DetailedTask,
    envelope_fields: &[&str],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    // The outer serde struct consumes its named fields before deserializing
    // the flattened task. Apply the same ownership when serializing.
    if envelope_fields
        .iter()
        .any(|key| task.additional().contains_key(*key))
    {
        let mut task = task.clone();
        for key in envelope_fields {
            task.additional_mut().remove(*key);
        }
        task.serialize(serializer)
    } else {
        task.serialize(serializer)
    }
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

    fn extra_fields() -> Map<String, Value> {
        Map::from_iter([
            ("example/object".into(), json!({"_meta": [null, true, 2.5]})),
            ("example/array".into(), json!([{}, "item", 42])),
            ("example/string".into(), json!("note")),
            ("example/bool".into(), json!(false)),
            ("example/number".into(), json!(7)),
            ("example/null".into(), Value::Null),
            (String::new(), json!("empty key")),
        ])
    }

    fn assert_round_trip<T: Serialize + for<'de> Deserialize<'de>>(typed: &T, expected: &Value) {
        let wire = serde_json::to_string(typed).unwrap();
        assert_eq!(&serde_json::from_str::<Value>(&wire).unwrap(), expected);
        // Decoding the actual text catches duplicate known keys that Value
        // would overwrite when it constructs an object.
        let decoded = serde_json::from_str::<T>(&wire).unwrap();
        assert_eq!(&serde_json::to_value(decoded).unwrap(), expected);
    }

    #[test]
    fn create_task_additional_fields_round_trip_without_collisions() {
        let mut expected = serde_json::to_value(CreateTaskResult::new(Task::new(
            metadata(),
            TaskStatus::Working,
        )))
        .unwrap();
        expected.as_object_mut().unwrap().extend(extra_fields());
        let mut result: CreateTaskResult = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(result.additional, extra_fields());
        for key in ["resultType", "_meta", "taskId", "status", "statusMessage"] {
            result.additional.insert(key.into(), json!("collision"));
        }
        assert_round_trip(&result, &expected);

        expected["_meta"] = json!({"example/request": "a"});
        result.meta = Some(expected["_meta"].as_object().unwrap().clone());
        assert_round_trip(&result, &expected);
    }

    #[test]
    fn detailed_task_fields_round_trip_alone_and_inside_envelopes() {
        let requests = InputRequests::from([(
            "roots".into(),
            InputRequest::ListRoots(ListRootsParams { meta: None }),
        )]);
        for task in [
            DetailedTask::working(metadata()),
            DetailedTask::input_required(metadata(), requests),
            DetailedTask::completed(
                metadata(),
                Map::from_iter([("isError".into(), json!(true))]),
            ),
            DetailedTask::failed(metadata(), JsonRpcError::internal_error("task failed")),
            DetailedTask::cancelled(metadata()),
        ] {
            let mut bare = serde_json::to_value(task).unwrap();
            bare.as_object_mut().unwrap().extend(extra_fields());
            let mut get = bare.clone();
            get["resultType"] = json!("complete");
            get["_meta"] = json!({"example/request": "b"});
            let mut notification = bare.clone();
            notification["_meta"] = json!({"example/event": "updated"});

            let mut decoded_get: GetTaskResult = serde_json::from_value(get.clone()).unwrap();
            let mut decoded_notification: TaskStatusNotificationParams =
                serde_json::from_value(notification.clone()).unwrap();
            assert_eq!(decoded_get.task.additional(), &extra_fields());
            assert_eq!(decoded_notification.task.additional(), &extra_fields());

            bare["resultType"] = json!("example/standalone");
            bare["_meta"] = json!([null]);
            let mut decoded_bare: DetailedTask = serde_json::from_value(bare.clone()).unwrap();
            assert_eq!(decoded_bare.additional()["resultType"], bare["resultType"]);
            assert_eq!(decoded_bare.additional()["_meta"], bare["_meta"]);

            for key in [
                "status",
                "taskId",
                "statusMessage",
                "inputRequests",
                "result",
                "error",
            ] {
                decoded_bare
                    .additional_mut()
                    .insert(key.into(), Value::Null);
            }
            assert_round_trip(&decoded_bare, &bare);

            decoded_get.task = decoded_bare.clone();
            assert_round_trip(&decoded_get, &get);
            decoded_get.meta = None;
            get.as_object_mut().unwrap().remove("_meta");
            assert_round_trip(&decoded_get, &get);

            decoded_notification.task = decoded_bare;
            notification["resultType"] = bare["resultType"].clone();
            assert_round_trip(&decoded_notification, &notification);
            decoded_notification.meta = None;
            notification.as_object_mut().unwrap().remove("_meta");
            assert_round_trip(&decoded_notification, &notification);
        }
    }

    #[test]
    fn additional_fields_do_not_bypass_typed_validation() {
        let base = serde_json::to_value(DetailedTask::working(metadata())).unwrap();
        for (status, field) in [
            ("input_required", "inputRequests"),
            ("completed", "result"),
            ("failed", "error"),
        ] {
            let mut invalid = base.clone();
            invalid["status"] = json!(status);
            invalid[field] = json!("invalid payload");
            assert!(serde_json::from_value::<DetailedTask>(invalid.clone()).is_err());
            assert!(
                serde_json::from_value::<TaskStatusNotificationParams>(invalid.clone()).is_err()
            );
            invalid["resultType"] = json!("complete");
            assert!(serde_json::from_value::<GetTaskResult>(invalid).is_err());
        }

        let mut invalid = base.clone();
        invalid["resultType"] = json!("task");
        assert!(
            serde_json::from_value::<GetTaskResult>(invalid.clone())
                .unwrap_err()
                .to_string()
                .contains("resultType must be \"complete\"")
        );
        invalid["_meta"] = json!([]);
        assert!(serde_json::from_value::<CreateTaskResult>(invalid.clone()).is_err());
        invalid["resultType"] = json!("complete");
        assert!(serde_json::from_value::<GetTaskResult>(invalid.clone()).is_err());
        assert!(serde_json::from_value::<TaskStatusNotificationParams>(invalid).is_err());
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
