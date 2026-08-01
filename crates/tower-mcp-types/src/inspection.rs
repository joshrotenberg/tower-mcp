//! Semantic inspection of JSON-RPC wire values.
//!
//! The protocol module exposes convenient request- and response-oriented
//! types. Intermediaries see a broader surface: either peer can send calls or
//! responses, and the 2025-03-26 MCP revision can carry batches. This module
//! classifies that complete surface without depending on a transport or async
//! runtime.
//!
//! # Example
//!
//! ```rust
//! use tower_mcp_types::inspection::{JsonRpcEnvelope, JsonRpcPayload};
//!
//! let wire = serde_json::json!({
//!     "jsonrpc": "2.0",
//!     "id": 1,
//!     "method": "tools/call",
//!     "params": {"name": "weather", "arguments": {"city": "Seattle"}}
//! });
//!
//! let payload = JsonRpcPayload::inspect(&wire)?;
//! let JsonRpcPayload::Single(JsonRpcEnvelope::Request(request)) = payload else {
//!     panic!("expected one request");
//! };
//! assert_eq!(request.method, "tools/call");
//! # Ok::<(), tower_mcp_types::inspection::JsonRpcInspectionError>(())
//! ```
//!
//! # Trust boundary
//!
//! Inspection starts from a [`serde_json::Value`]. Parsing JSON into a value
//! has already collapsed duplicate object keys. An intermediary that inspects
//! a value and then forwards the original bytes must separately prevent
//! duplicate-key and parser-differential attacks. It must also bound input
//! size and nesting before parsing.
//!
//! The returned types contain the protocol fields represented by
//! [`crate::protocol`]. Keep the original value when extension fields or the
//! exact input representation are needed. Because inspection returns the
//! crate's existing owned types, numeric request IDs must fit in `i64` and
//! error codes must fit in `i32`.

use serde_json::{Map, Value};

use crate::protocol::{
    JSONRPC_VERSION, JsonRpcErrorResponse, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResultResponse,
};

/// The semantic kind of one JSON-RPC envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JsonRpcEnvelopeKind {
    /// A request carrying an ID and expecting a response.
    Request,
    /// A notification with no ID or response.
    Notification,
    /// A successful response.
    Result,
    /// An error response.
    Error,
}

impl std::fmt::Display for JsonRpcEnvelopeKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Request => "request",
            Self::Notification => "notification",
            Self::Result => "result response",
            Self::Error => "error response",
        };
        formatter.write_str(name)
    }
}

/// One structurally valid JSON-RPC 2.0 envelope.
///
/// This is intentionally separate from [`crate::protocol::JsonRpcMessage`],
/// whose request-only shape is part of the `tower-mcp` dispatch API.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum JsonRpcEnvelope {
    /// A request carrying an ID and expecting a response.
    Request(JsonRpcRequest),
    /// A notification with no ID or response.
    Notification(JsonRpcNotification),
    /// A successful response.
    Result(JsonRpcResultResponse),
    /// An error response.
    ///
    /// For compatibility with MCP transport-level failures, inspection accepts
    /// either a missing `id` or an explicit null `id`. Both forms become
    /// [`JsonRpcErrorResponse::id`] `None`; retain the original value when the
    /// distinction matters.
    Error(JsonRpcErrorResponse),
}

impl JsonRpcEnvelope {
    /// Return the envelope's semantic kind.
    #[must_use]
    pub const fn kind(&self) -> JsonRpcEnvelopeKind {
        match self {
            Self::Request(_) => JsonRpcEnvelopeKind::Request,
            Self::Notification(_) => JsonRpcEnvelopeKind::Notification,
            Self::Result(_) => JsonRpcEnvelopeKind::Result,
            Self::Error(_) => JsonRpcEnvelopeKind::Error,
        }
    }

    const fn batch_kind(&self) -> JsonRpcBatchKind {
        match self {
            Self::Request(_) | Self::Notification(_) => JsonRpcBatchKind::Calls,
            Self::Result(_) | Self::Error(_) => JsonRpcBatchKind::Responses,
        }
    }
}

/// The side of a JSON-RPC batch represented by its members.
///
/// JSON-RPC call batches may mix requests and notifications. Response batches
/// may mix success and error responses. A batch cannot mix calls and
/// responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JsonRpcBatchKind {
    /// Requests and notifications sent by a caller.
    Calls,
    /// Success and error responses sent by a receiver.
    Responses,
}

impl std::fmt::Display for JsonRpcBatchKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Calls => formatter.write_str("calls"),
            Self::Responses => formatter.write_str("responses"),
        }
    }
}

/// A non-empty JSON-RPC batch whose members are all on the same batch side.
///
/// The fields are private so the invariants established by
/// [`JsonRpcPayload::inspect`] cannot be bypassed accidentally.
#[derive(Debug, Clone)]
pub struct JsonRpcBatch {
    kind: JsonRpcBatchKind,
    messages: Vec<JsonRpcEnvelope>,
}

impl JsonRpcBatch {
    /// Return whether this is a call-side or response-side batch.
    #[must_use]
    pub const fn kind(&self) -> JsonRpcBatchKind {
        self.kind
    }

    /// Return the inspected messages in wire order.
    #[must_use]
    pub fn messages(&self) -> &[JsonRpcEnvelope] {
        &self.messages
    }

    /// Consume the batch and return its inspected messages in wire order.
    #[must_use]
    pub fn into_messages(self) -> Vec<JsonRpcEnvelope> {
        self.messages
    }

    /// Return the number of messages in this batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Batches produced by inspection are always non-empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }
}

/// A structurally valid single JSON-RPC envelope or batch.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum JsonRpcPayload {
    /// One request, notification, success response, or error response.
    Single(JsonRpcEnvelope),
    /// A non-empty call-side or response-side batch.
    Batch(JsonRpcBatch),
}

impl JsonRpcPayload {
    /// Inspect an already-decoded JSON value as a JSON-RPC 2.0 payload.
    ///
    /// This performs structural JSON-RPC validation. It does not select an MCP
    /// protocol revision, validate MCP method parameters, correlate responses,
    /// or enforce transport and lifecycle rules.
    pub fn inspect(value: &Value) -> Result<Self, JsonRpcInspectionError> {
        match value {
            Value::Object(_) => inspect_envelope(value).map(Self::Single),
            Value::Array(items) => inspect_batch(items).map(Self::Batch),
            _ => Err(JsonRpcInspectionError::new(
                JsonRpcInspectionErrorKind::InvalidTopLevel,
                None,
                "JSON-RPC payload must be an object or array",
            )),
        }
    }

    /// Return whether this payload is a batch.
    #[must_use]
    pub const fn is_batch(&self) -> bool {
        matches!(self, Self::Batch(_))
    }

    /// Return the number of envelopes in the payload.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Single(_) => 1,
            Self::Batch(batch) => batch.len(),
        }
    }

    /// A valid payload is never empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Return the single envelope, or `None` for a batch.
    #[must_use]
    pub const fn as_single(&self) -> Option<&JsonRpcEnvelope> {
        match self {
            Self::Single(envelope) => Some(envelope),
            Self::Batch(_) => None,
        }
    }

    /// Return the batch, or `None` for a single envelope.
    #[must_use]
    pub const fn as_batch(&self) -> Option<&JsonRpcBatch> {
        match self {
            Self::Single(_) => None,
            Self::Batch(batch) => Some(batch),
        }
    }
}

/// Stable category for a JSON-RPC inspection failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JsonRpcInspectionErrorKind {
    /// The top-level value was neither an object nor an array.
    InvalidTopLevel,
    /// A batch member was not a JSON-RPC object.
    InvalidBatchMember,
    /// A batch contained no members.
    EmptyBatch,
    /// A batch mixed calls with responses.
    MixedBatch,
    /// A required field was absent.
    MissingField,
    /// A field had the wrong JSON type or value domain.
    InvalidField,
    /// The `jsonrpc` member was not exactly `"2.0"`.
    InvalidVersion,
    /// Mutually exclusive envelope members appeared together.
    ConflictingFields,
    /// The object did not identify any JSON-RPC envelope kind.
    InvalidEnvelope,
}

/// A structured failure returned while inspecting a JSON-RPC value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{detail}")]
#[non_exhaustive]
pub struct JsonRpcInspectionError {
    kind: JsonRpcInspectionErrorKind,
    batch_index: Option<usize>,
    field: Option<&'static str>,
    detail: String,
}

impl JsonRpcInspectionError {
    fn new(
        kind: JsonRpcInspectionErrorKind,
        field: Option<&'static str>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            batch_index: None,
            field,
            detail: detail.into(),
        }
    }

    fn at_batch_index(mut self, index: usize) -> Self {
        self.detail = format!("batch item {index}: {}", self.detail);
        self.batch_index = Some(index);
        self
    }

    /// Return the stable error category for typed control flow.
    #[must_use]
    pub const fn kind(&self) -> JsonRpcInspectionErrorKind {
        self.kind
    }

    /// Return the zero-based batch-member index, when the failure occurred in
    /// a batch.
    #[must_use]
    pub const fn batch_index(&self) -> Option<usize> {
        self.batch_index
    }

    /// Return the relevant JSON-RPC field, when the failure concerns one
    /// field.
    #[must_use]
    pub const fn field(&self) -> Option<&'static str> {
        self.field
    }

    /// Return the human-readable diagnostic.
    ///
    /// Use [`Self::kind`], [`Self::batch_index`], and [`Self::field`] rather
    /// than parsing this text for control flow.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

fn inspect_batch(items: &[Value]) -> Result<JsonRpcBatch, JsonRpcInspectionError> {
    if items.is_empty() {
        return Err(JsonRpcInspectionError::new(
            JsonRpcInspectionErrorKind::EmptyBatch,
            None,
            "JSON-RPC batch must contain at least one message",
        ));
    }

    let mut messages = Vec::with_capacity(items.len());
    let mut kind = None;

    for (index, item) in items.iter().enumerate() {
        item.as_object().ok_or_else(|| {
            JsonRpcInspectionError::new(
                JsonRpcInspectionErrorKind::InvalidBatchMember,
                None,
                "JSON-RPC batch member must be an object",
            )
            .at_batch_index(index)
        })?;
        let envelope = inspect_envelope(item).map_err(|error| error.at_batch_index(index))?;
        let member_kind = envelope.batch_kind();

        if let Some(expected) = kind {
            if member_kind != expected {
                return Err(JsonRpcInspectionError::new(
                    JsonRpcInspectionErrorKind::MixedBatch,
                    None,
                    format!("JSON-RPC batch cannot mix {expected} with {member_kind}"),
                )
                .at_batch_index(index));
            }
        } else {
            kind = Some(member_kind);
        }

        messages.push(envelope);
    }

    Ok(JsonRpcBatch {
        // The empty case returned above, so the first member always sets kind.
        kind: kind.expect("non-empty batch has a kind"),
        messages,
    })
}

fn inspect_envelope(value: &Value) -> Result<JsonRpcEnvelope, JsonRpcInspectionError> {
    let object = value
        .as_object()
        .expect("inspection only passes JSON objects as envelopes");
    inspect_version(object)?;

    let has_method = object.contains_key("method");
    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");

    if has_method && (has_result || has_error) {
        return Err(JsonRpcInspectionError::new(
            JsonRpcInspectionErrorKind::ConflictingFields,
            Some("method"),
            "JSON-RPC envelope cannot contain both `method` and `result`/`error`",
        ));
    }
    if has_result && has_error {
        return Err(JsonRpcInspectionError::new(
            JsonRpcInspectionErrorKind::ConflictingFields,
            Some("result"),
            "JSON-RPC response cannot contain both `result` and `error`",
        ));
    }

    if has_method {
        inspect_method(object)?;
        inspect_params(object)?;
        return if object.contains_key("id") {
            inspect_non_null_id(object, "request")?;
            decode_value::<JsonRpcRequest>(value, "request").map(JsonRpcEnvelope::Request)
        } else {
            decode_value::<JsonRpcNotification>(value, "notification")
                .map(JsonRpcEnvelope::Notification)
        };
    }

    if has_result {
        inspect_non_null_id(object, "success response")?;
        return decode_value::<JsonRpcResultResponse>(value, "success response")
            .map(JsonRpcEnvelope::Result);
    }

    if has_error {
        inspect_optional_error_id(object)?;
        return decode_value::<JsonRpcErrorResponse>(value, "error response")
            .map(JsonRpcEnvelope::Error);
    }

    Err(JsonRpcInspectionError::new(
        JsonRpcInspectionErrorKind::InvalidEnvelope,
        None,
        "JSON-RPC object must contain `method`, `result`, or `error`",
    ))
}

fn inspect_params(object: &Map<String, Value>) -> Result<(), JsonRpcInspectionError> {
    match object.get("params") {
        None | Some(Value::Object(_) | Value::Array(_)) => Ok(()),
        Some(_) => Err(JsonRpcInspectionError::new(
            JsonRpcInspectionErrorKind::InvalidField,
            Some("params"),
            "JSON-RPC `params` field must be an object or array when present",
        )),
    }
}

fn inspect_version(object: &Map<String, Value>) -> Result<(), JsonRpcInspectionError> {
    match object.get("jsonrpc") {
        None => Err(JsonRpcInspectionError::new(
            JsonRpcInspectionErrorKind::MissingField,
            Some("jsonrpc"),
            "JSON-RPC envelope is missing `jsonrpc`",
        )),
        Some(Value::String(version)) if version == JSONRPC_VERSION => Ok(()),
        Some(Value::String(version)) => Err(JsonRpcInspectionError::new(
            JsonRpcInspectionErrorKind::InvalidVersion,
            Some("jsonrpc"),
            format!("JSON-RPC version must be `{JSONRPC_VERSION}`, got `{version}`"),
        )),
        Some(_) => Err(JsonRpcInspectionError::new(
            JsonRpcInspectionErrorKind::InvalidField,
            Some("jsonrpc"),
            "JSON-RPC `jsonrpc` field must be a string",
        )),
    }
}

fn inspect_method(object: &Map<String, Value>) -> Result<(), JsonRpcInspectionError> {
    match object.get("method") {
        Some(Value::String(method)) if !method.is_empty() => Ok(()),
        Some(Value::String(_)) => Err(JsonRpcInspectionError::new(
            JsonRpcInspectionErrorKind::InvalidField,
            Some("method"),
            "JSON-RPC `method` field must not be empty",
        )),
        Some(_) => Err(JsonRpcInspectionError::new(
            JsonRpcInspectionErrorKind::InvalidField,
            Some("method"),
            "JSON-RPC `method` field must be a string",
        )),
        None => Err(JsonRpcInspectionError::new(
            JsonRpcInspectionErrorKind::MissingField,
            Some("method"),
            "JSON-RPC call is missing `method`",
        )),
    }
}

fn inspect_non_null_id(
    object: &Map<String, Value>,
    context: &'static str,
) -> Result<(), JsonRpcInspectionError> {
    let id = object.get("id").ok_or_else(|| {
        JsonRpcInspectionError::new(
            JsonRpcInspectionErrorKind::MissingField,
            Some("id"),
            format!("JSON-RPC {context} is missing `id`"),
        )
    })?;

    if valid_request_id(id) {
        Ok(())
    } else {
        Err(JsonRpcInspectionError::new(
            JsonRpcInspectionErrorKind::InvalidField,
            Some("id"),
            format!("JSON-RPC {context} `id` must be a string or signed integer"),
        ))
    }
}

fn inspect_optional_error_id(object: &Map<String, Value>) -> Result<(), JsonRpcInspectionError> {
    let Some(id) = object.get("id") else {
        return Ok(());
    };
    if id.is_null() || valid_request_id(id) {
        Ok(())
    } else {
        Err(JsonRpcInspectionError::new(
            JsonRpcInspectionErrorKind::InvalidField,
            Some("id"),
            "JSON-RPC error response `id` must be null, a string, or a signed integer",
        ))
    }
}

fn valid_request_id(value: &Value) -> bool {
    matches!(value, Value::String(_)) || value.as_i64().is_some()
}

fn decode_value<T>(value: &Value, context: &'static str) -> Result<T, JsonRpcInspectionError>
where
    T: serde::de::DeserializeOwned,
{
    T::deserialize(value).map_err(|error| {
        JsonRpcInspectionError::new(
            JsonRpcInspectionErrorKind::InvalidField,
            decode_error_field(context),
            format!("invalid JSON-RPC {context}: {error}"),
        )
    })
}

fn decode_error_field(context: &'static str) -> Option<&'static str> {
    if context == "error response" {
        Some("error")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::protocol::{JsonRpcMessage, JsonRpcResponseMessage};

    fn inspect(value: Value) -> Result<JsonRpcPayload, JsonRpcInspectionError> {
        JsonRpcPayload::inspect(&value)
    }

    #[test]
    fn classifies_all_four_envelope_kinds() {
        let cases = [
            (
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
                JsonRpcEnvelopeKind::Request,
            ),
            (
                json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                JsonRpcEnvelopeKind::Notification,
            ),
            (
                json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
                JsonRpcEnvelopeKind::Result,
            ),
            (
                json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "missing"}}),
                JsonRpcEnvelopeKind::Error,
            ),
        ];

        for (value, expected) in cases {
            let payload = inspect(value).unwrap();
            assert_eq!(payload.as_single().unwrap().kind(), expected);
        }
    }

    #[test]
    fn classifies_call_and_response_batches() {
        let calls = inspect(json!([
            {"jsonrpc": "2.0", "id": 1, "method": "tools/list"},
            {"jsonrpc": "2.0", "method": "notifications/initialized"}
        ]))
        .unwrap();
        let calls = calls.as_batch().unwrap();
        assert_eq!(calls.kind(), JsonRpcBatchKind::Calls);
        assert_eq!(calls.len(), 2);
        assert!(!calls.is_empty());

        let responses = inspect(json!([
            {"jsonrpc": "2.0", "id": 1, "result": {}},
            {"jsonrpc": "2.0", "id": 2, "error": {"code": -32601, "message": "missing"}}
        ]))
        .unwrap();
        assert_eq!(
            responses.as_batch().unwrap().kind(),
            JsonRpcBatchKind::Responses
        );
    }

    #[test]
    fn rejects_empty_and_mixed_batches() {
        let empty = inspect(json!([])).unwrap_err();
        assert_eq!(empty.kind(), JsonRpcInspectionErrorKind::EmptyBatch);
        assert_eq!(empty.batch_index(), None);

        let mixed = inspect(json!([
            {"jsonrpc": "2.0", "id": 1, "method": "tools/list"},
            {"jsonrpc": "2.0", "id": 1, "result": {}}
        ]))
        .unwrap_err();
        assert_eq!(mixed.kind(), JsonRpcInspectionErrorKind::MixedBatch);
        assert_eq!(mixed.batch_index(), Some(1));
    }

    #[test]
    fn reports_index_for_invalid_batch_member() {
        let error = inspect(json!([
            {"jsonrpc": "2.0", "id": 1, "method": "tools/list"},
            {"id": 2, "method": "tools/call"}
        ]))
        .unwrap_err();
        assert_eq!(error.kind(), JsonRpcInspectionErrorKind::MissingField);
        assert_eq!(error.field(), Some("jsonrpc"));
        assert_eq!(error.batch_index(), Some(1));
    }

    #[test]
    fn rejects_invalid_top_level_and_nested_batch() {
        let scalar = inspect(json!(true)).unwrap_err();
        assert_eq!(scalar.kind(), JsonRpcInspectionErrorKind::InvalidTopLevel);

        let nested = inspect(json!([[{"jsonrpc": "2.0", "id": 1, "method": "ping"}]])).unwrap_err();
        assert_eq!(
            nested.kind(),
            JsonRpcInspectionErrorKind::InvalidBatchMember
        );
        assert_eq!(nested.batch_index(), Some(0));
    }

    #[test]
    fn params_must_be_an_object_or_array_when_present() {
        for valid in [
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {}}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": []}),
        ] {
            assert!(inspect(valid).is_ok());
        }

        for invalid in [
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": null}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": true}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": 42}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": "value"}),
        ] {
            let error = inspect(invalid).unwrap_err();
            assert_eq!(error.kind(), JsonRpcInspectionErrorKind::InvalidField);
            assert_eq!(error.field(), Some("params"));
        }
    }

    #[test]
    fn rejects_missing_wrong_and_non_string_versions() {
        let missing = inspect(json!({"id": 1, "method": "ping"})).unwrap_err();
        assert_eq!(missing.kind(), JsonRpcInspectionErrorKind::MissingField);
        assert_eq!(missing.field(), Some("jsonrpc"));

        let wrong = inspect(json!({"jsonrpc": "1.0", "id": 1, "method": "ping"})).unwrap_err();
        assert_eq!(wrong.kind(), JsonRpcInspectionErrorKind::InvalidVersion);

        let non_string = inspect(json!({"jsonrpc": 2, "id": 1, "method": "ping"})).unwrap_err();
        assert_eq!(non_string.kind(), JsonRpcInspectionErrorKind::InvalidField);
        assert_eq!(non_string.field(), Some("jsonrpc"));
    }

    #[test]
    fn rejects_invalid_methods_and_ids() {
        for value in [
            json!({"jsonrpc": "2.0", "id": 1, "method": 7}),
            json!({"jsonrpc": "2.0", "id": 1, "method": ""}),
            json!({"jsonrpc": "2.0", "id": null, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": 1.5, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": true, "result": {}}),
        ] {
            assert_eq!(
                inspect(value).unwrap_err().kind(),
                JsonRpcInspectionErrorKind::InvalidField
            );
        }
    }

    #[test]
    fn rejects_conflicting_and_unidentified_envelopes() {
        for value in [
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "result": {}}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "error": {"code": -1, "message": "x"}}),
            json!({"jsonrpc": "2.0", "id": 1, "result": {}, "error": {"code": -1, "message": "x"}}),
        ] {
            assert_eq!(
                inspect(value).unwrap_err().kind(),
                JsonRpcInspectionErrorKind::ConflictingFields
            );
        }

        let unidentified = inspect(json!({"jsonrpc": "2.0", "id": 1})).unwrap_err();
        assert_eq!(
            unidentified.kind(),
            JsonRpcInspectionErrorKind::InvalidEnvelope
        );
    }

    #[test]
    fn rejects_malformed_error_objects() {
        for value in [
            json!({"jsonrpc": "2.0", "id": 1, "error": null}),
            json!({"jsonrpc": "2.0", "id": 1, "error": {"message": "x"}}),
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": "-1", "message": "x"}}),
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -1, "message": 7}}),
        ] {
            let error = inspect(value).unwrap_err();
            assert_eq!(error.kind(), JsonRpcInspectionErrorKind::InvalidField);
            assert_eq!(error.field(), Some("error"));
        }
    }

    #[test]
    fn error_response_allows_absent_or_null_id() {
        for value in [
            json!({"jsonrpc": "2.0", "error": {"code": -32700, "message": "parse error"}}),
            json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}),
        ] {
            assert!(matches!(
                inspect(value).unwrap().as_single(),
                Some(JsonRpcEnvelope::Error(_))
            ));
        }
    }

    #[test]
    fn request_and_result_require_non_null_id() {
        for value in [
            json!({"jsonrpc": "2.0", "id": null, "method": "ping"}),
            json!({"jsonrpc": "2.0", "result": {}}),
            json!({"jsonrpc": "2.0", "id": null, "result": {}}),
        ] {
            assert!(inspect(value).is_err());
        }
    }

    #[test]
    fn numeric_fields_must_fit_existing_owned_types() {
        let large_id = inspect(json!({
            "jsonrpc": "2.0",
            "id": u64::MAX,
            "method": "ping"
        }))
        .unwrap_err();
        assert_eq!(large_id.kind(), JsonRpcInspectionErrorKind::InvalidField);
        assert_eq!(large_id.field(), Some("id"));

        let large_code = inspect(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": i64::MAX, "message": "outside i32"}
        }))
        .unwrap_err();
        assert_eq!(large_code.kind(), JsonRpcInspectionErrorKind::InvalidField);
        assert_eq!(large_code.field(), Some("error"));
    }

    #[test]
    fn unknown_methods_and_extension_fields_are_allowed() {
        let value = json!({
            "jsonrpc": "2.0",
            "id": "custom",
            "method": "com.example/widgets/do",
            "params": {"vendor": true},
            "com.example/trace": {"id": "abc"}
        });
        let payload = JsonRpcPayload::inspect(&value).unwrap();
        let Some(JsonRpcEnvelope::Request(request)) = payload.as_single() else {
            panic!("expected a request");
        };
        assert_eq!(request.method, "com.example/widgets/do");
        assert!(value.get("com.example/trace").is_some());
    }

    #[test]
    fn existing_dispatch_message_serde_is_unchanged() {
        let single: JsonRpcMessage = serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 1, "method": "ping"
        }))
        .unwrap();
        assert!(!single.is_batch());

        let batch: JsonRpcMessage = serde_json::from_value(json!([
            {"jsonrpc": "2.0", "id": 1, "method": "ping"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}
        ]))
        .unwrap();
        assert!(batch.is_batch());

        let responses: JsonRpcResponseMessage = serde_json::from_value(json!([
            {"jsonrpc": "2.0", "id": 1, "result": {}},
            {"jsonrpc": "2.0", "id": 2, "error": {"code": -32601, "message": "missing"}}
        ]))
        .unwrap();
        assert!(responses.is_batch());
    }

    #[test]
    fn accessors_cover_single_and_batch_payloads() {
        let single = inspect(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})).unwrap();
        assert_eq!(single.len(), 1);
        assert!(!single.is_empty());
        assert!(!single.is_batch());
        assert!(single.as_batch().is_none());

        let batch = inspect(json!([
            {"jsonrpc": "2.0", "id": 1, "method": "ping"},
            {"jsonrpc": "2.0", "method": "notifications/initialized"}
        ]))
        .unwrap();
        assert_eq!(batch.len(), 2);
        assert!(batch.is_batch());
        assert!(batch.as_single().is_none());
        let owned = match batch {
            JsonRpcPayload::Batch(batch) => batch.into_messages(),
            JsonRpcPayload::Single(_) => unreachable!(),
        };
        assert_eq!(owned.len(), 2);
    }
}
