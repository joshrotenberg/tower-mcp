//! Wire-format assertion helpers for JSON-RPC test cases.
//!
//! These helpers panic with descriptive messages when a `serde_json::Value`
//! violates a JSON-RPC 2.0 invariant — the kind of check that catches
//! "field omitted instead of `null`" or "missing `jsonrpc` version" bugs in
//! one line at the test site, without each test re-stating the spec.
//!
//! Available behind the `testing` feature.
//!
//! ```
//! # #[cfg(feature = "testing")] {
//! use tower_mcp_types::testing::assert_jsonrpc_error_response;
//! let wire = serde_json::json!({
//!     "jsonrpc": "2.0",
//!     "id": null,
//!     "error": {"code": -32700, "message": "Parse error"}
//! });
//! assert_jsonrpc_error_response(&wire);
//! # }
//! ```
//!
//! Each function asserts a single shape — `_response` for either flavor,
//! `_error_response` / `_result_response` for the specific variant, and
//! `_request` for outbound or inbound requests.

use serde_json::Value;

/// Assert a value is a valid JSON-RPC 2.0 response (either result or error).
///
/// Panics if `jsonrpc` is missing or not `"2.0"`, if neither `result` nor
/// `error` is present, or if both are present.
pub fn assert_jsonrpc_response(v: &Value) {
    assert_jsonrpc_version(v);
    let has_result = v.get("result").is_some();
    let has_error = v.get("error").is_some();
    assert!(
        has_result ^ has_error,
        "JSON-RPC response must contain exactly one of `result` or `error`, got: {v}"
    );
    if has_error {
        assert_jsonrpc_error_response(v);
    } else {
        assert_jsonrpc_result_response(v);
    }
}

/// Assert a value is a valid JSON-RPC 2.0 error response.
///
/// Panics if `jsonrpc` is wrong, `id` is missing (must be present, may be
/// `null`), `error.code` is missing or not an integer, or `error.message`
/// is missing or empty.
pub fn assert_jsonrpc_error_response(v: &Value) {
    assert_jsonrpc_version(v);
    assert!(
        v.get("id").is_some(),
        "JSON-RPC error response MUST contain `id` (use null when unknown), got: {v}"
    );
    let id = &v["id"];
    assert!(
        id.is_null() || id.is_string() || id.is_i64() || id.is_u64(),
        "JSON-RPC error response `id` must be null, string, or integer, got: {id}"
    );
    let error = v
        .get("error")
        .unwrap_or_else(|| panic!("JSON-RPC error response missing `error` object: {v}"));
    let code = error
        .get("code")
        .unwrap_or_else(|| panic!("JSON-RPC error.code missing: {v}"));
    assert!(
        code.is_i64() || code.is_u64(),
        "JSON-RPC error.code must be an integer, got: {code}"
    );
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("JSON-RPC error.message must be a string: {v}"));
    assert!(
        !message.is_empty(),
        "JSON-RPC error.message must be non-empty: {v}"
    );
}

/// Assert a value is a valid JSON-RPC 2.0 success response.
///
/// Panics if `jsonrpc` is wrong, `id` is missing or null (success responses
/// must echo the request id), or `result` is missing.
pub fn assert_jsonrpc_result_response(v: &Value) {
    assert_jsonrpc_version(v);
    let id = v
        .get("id")
        .unwrap_or_else(|| panic!("JSON-RPC result response missing `id`: {v}"));
    assert!(
        id.is_string() || id.is_i64() || id.is_u64(),
        "JSON-RPC result response `id` must be string or integer (not null), got: {id}"
    );
    assert!(
        v.get("result").is_some(),
        "JSON-RPC result response missing `result`: {v}"
    );
}

/// Assert a value is a valid JSON-RPC 2.0 request.
///
/// Panics if `jsonrpc` is wrong, `id` is missing or null, or `method` is
/// missing/empty. For notifications (no id), use [`assert_jsonrpc_notification`].
pub fn assert_jsonrpc_request(v: &Value) {
    assert_jsonrpc_version(v);
    let id = v.get("id").unwrap_or_else(|| {
        panic!("JSON-RPC request missing `id` (use notification helper for fire-and-forget): {v}")
    });
    assert!(
        id.is_string() || id.is_i64() || id.is_u64(),
        "JSON-RPC request `id` must be string or integer, got: {id}"
    );
    assert_method(v);
}

/// Assert a value is a valid JSON-RPC 2.0 notification (no `id` field).
pub fn assert_jsonrpc_notification(v: &Value) {
    assert_jsonrpc_version(v);
    assert!(
        v.get("id").is_none(),
        "JSON-RPC notification must NOT contain `id`, got: {v}"
    );
    assert_method(v);
}

fn assert_jsonrpc_version(v: &Value) {
    let version = v
        .get("jsonrpc")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing or non-string `jsonrpc` field: {v}"));
    assert_eq!(
        version, "2.0",
        "JSON-RPC version must be \"2.0\", got: {version}"
    );
}

fn assert_method(v: &Value) {
    let method = v
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("JSON-RPC `method` must be a string: {v}"));
    assert!(
        !method.is_empty(),
        "JSON-RPC `method` must be non-empty: {v}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn error_response_with_null_id_ok() {
        assert_jsonrpc_error_response(&json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {"code": -32700, "message": "Parse error"}
        }));
    }

    #[test]
    fn error_response_with_int_id_ok() {
        assert_jsonrpc_error_response(&json!({
            "jsonrpc": "2.0",
            "id": 7,
            "error": {"code": -32601, "message": "Method not found"}
        }));
    }

    #[test]
    #[should_panic(expected = "MUST contain `id`")]
    fn error_response_missing_id_panics() {
        assert_jsonrpc_error_response(&json!({
            "jsonrpc": "2.0",
            "error": {"code": -32700, "message": "x"}
        }));
    }

    #[test]
    #[should_panic(expected = "error.message must be non-empty")]
    fn error_response_empty_message_panics() {
        assert_jsonrpc_error_response(&json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {"code": -32700, "message": ""}
        }));
    }

    #[test]
    #[should_panic(expected = "JSON-RPC version must be")]
    fn wrong_version_panics() {
        assert_jsonrpc_error_response(&json!({
            "jsonrpc": "1.0",
            "id": null,
            "error": {"code": -32700, "message": "x"}
        }));
    }

    #[test]
    fn response_dispatches_to_result_or_error() {
        assert_jsonrpc_response(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {}
        }));
        assert_jsonrpc_response(&json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {"code": -32700, "message": "x"}
        }));
    }

    #[test]
    #[should_panic(expected = "exactly one of `result` or `error`")]
    fn response_with_both_result_and_error_panics() {
        assert_jsonrpc_response(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {},
            "error": {"code": -32000, "message": "x"}
        }));
    }

    #[test]
    #[should_panic(expected = "result response `id` must be string or integer (not null)")]
    fn result_response_with_null_id_panics() {
        assert_jsonrpc_result_response(&json!({
            "jsonrpc": "2.0",
            "id": null,
            "result": {}
        }));
    }

    #[test]
    fn request_ok() {
        assert_jsonrpc_request(&json!({
            "jsonrpc": "2.0",
            "id": "abc",
            "method": "tools/list"
        }));
    }

    #[test]
    #[should_panic(expected = "JSON-RPC request missing `id`")]
    fn request_missing_id_panics() {
        assert_jsonrpc_request(&json!({
            "jsonrpc": "2.0",
            "method": "tools/list"
        }));
    }

    #[test]
    fn notification_ok() {
        assert_jsonrpc_notification(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }));
    }

    #[test]
    #[should_panic(expected = "notification must NOT contain `id`")]
    fn notification_with_id_panics() {
        assert_jsonrpc_notification(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "x"
        }));
    }
}
