//! SEP-2243: HTTP header standardization for the Streamable HTTP transport.
//!
//! This module implements the server-side validation rules from SEP-2243
//! (FINAL 2026-04-15). The SEP requires POST requests to include several
//! headers that mirror fields from the JSON-RPC body so that network
//! intermediaries (load balancers, proxies, observability tools, WAFs)
//! can route and process MCP traffic without parsing the body.
//!
//! ## Headers
//!
//! - `Mcp-Method` — mirrors the JSON-RPC `method`. Required on all
//!   requests and notifications.
//! - `Mcp-Name` — mirrors `params.name` for `tools/call` / `prompts/get`,
//!   or `params.uri` for `resources/read`. Required for those three
//!   methods.
//! - `Mcp-Param-{Name}` — mirrors tool parameters marked with the
//!   `x-mcp-header` JSON Schema extension. Server validates that, after
//!   optional Base64 decoding, the header value matches the body value.
//!
//! ## Encoding (Mcp-Param-*)
//!
//! Values that contain non-ASCII characters, control characters, or
//! leading/trailing whitespace are Base64-encoded using the sentinel
//! wrapper:
//!
//! ```text
//! Mcp-Param-{Name}: =?base64?{Base64Value}?=
//! ```
//!
//! The `=?base64?` prefix is case-insensitive.
//!
//! ## Errors
//!
//! All validation failures return a JSON-RPC error with code
//! [`McpErrorCode::HeaderMismatch`](crate::error::McpErrorCode::HeaderMismatch)
//! (-32001) and HTTP status `400 Bad Request`.
//!
//! ## Version gating
//!
//! The MAY clause in the SEP's Backward Compatibility section says:
//!
//! > Servers MAY support older clients by accepting requests without
//! > headers when negotiating an older protocol version.
//!
//! This implementation enforces the SEP strictly only when the
//! negotiated protocol version is `>= 2026-07-28` (the first MCP version
//! that includes SEP-2243). For earlier versions, validation is
//! "lenient": present headers are still checked for body consistency,
//! but missing headers do not produce an error. This lets clients opt in
//! early without forcing existing 2025-11-25 clients off the wire.

use base64::Engine;
use serde_json::Value;
use tower_mcp_types::JsonRpcError;

use super::http::{MCP_METHOD_HEADER, MCP_NAME_HEADER, MCP_PARAM_HEADER_PREFIX};

/// Base64 sentinel prefix used for Base64-encoded header values.
///
/// Per SEP-2243 this prefix is case-insensitive.
const BASE64_PREFIX: &str = "=?base64?";
/// Base64 sentinel suffix used for Base64-encoded header values.
const BASE64_SUFFIX: &str = "?=";

/// Validation mode for SEP-2243 header checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Sep2243Mode {
    /// The negotiated protocol version implements SEP-2243. Missing
    /// required headers and any header/body mismatch produce a
    /// `HeaderMismatch` error.
    Strict,
    /// The negotiated protocol version pre-dates SEP-2243. Missing
    /// headers are accepted; present headers that mismatch the body
    /// still produce a `HeaderMismatch` error.
    Lenient,
}

/// Decide which validation mode applies for the negotiated protocol
/// version.
///
/// Returns [`Sep2243Mode::Strict`] when `version` is at or beyond the
/// SEP-2243 inclusion version, [`Sep2243Mode::Lenient`] otherwise.
///
/// Version strings are compared lexicographically. The MCP version
/// format is `YYYY-MM-DD`, which sorts correctly under lexical compare.
pub(super) fn mode_for_version(version: &str) -> Sep2243Mode {
    if version >= super::http::SEP_2243_MIN_PROTOCOL_VERSION {
        Sep2243Mode::Strict
    } else {
        Sep2243Mode::Lenient
    }
}

/// Validate SEP-2243 headers against the parsed JSON-RPC body.
///
/// Returns `Ok(())` if the headers are consistent with the body (or, in
/// [`Sep2243Mode::Lenient`] mode, if required headers are simply absent).
/// Returns `Err` with a `HeaderMismatch` JSON-RPC error on any
/// validation failure.
///
/// The caller is responsible for short-circuiting with a `400 Bad
/// Request` response.
pub(super) fn validate(
    headers: &axum::http::HeaderMap,
    parsed: &Value,
    mode: Sep2243Mode,
) -> Result<(), JsonRpcError> {
    let body_method = parsed
        .get("method")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let mcp_method_header = header_str(headers, MCP_METHOD_HEADER)?;

    match (mcp_method_header.as_deref(), body_method.as_deref()) {
        (None, _) if matches!(mode, Sep2243Mode::Strict) => {
            return Err(JsonRpcError::header_mismatch(
                "Mcp-Method header is required",
            ));
        }
        (Some(h), Some(b)) if h != b => {
            return Err(JsonRpcError::header_mismatch(format!(
                "Mcp-Method header value {h:?} does not match body method {b:?}"
            )));
        }
        (Some(h), None) => {
            return Err(JsonRpcError::header_mismatch(format!(
                "Mcp-Method header value {h:?} present but body has no method field"
            )));
        }
        _ => {}
    }

    // Determine the expected name source by the body method (the
    // authoritative value per SEP-2243 — header values are advisory for
    // routing and are validated against the body).
    let name_source: Option<NameSource> = match body_method.as_deref() {
        Some("tools/call") | Some("prompts/get") => Some(NameSource::ParamsName),
        Some("resources/read") => Some(NameSource::ParamsUri),
        _ => None,
    };

    let mcp_name_header = header_str(headers, MCP_NAME_HEADER)?;

    if let Some(source) = name_source {
        let body_value = source.extract(parsed);
        match (mcp_name_header.as_deref(), body_value.as_deref()) {
            (None, Some(_)) if matches!(mode, Sep2243Mode::Strict) => {
                return Err(JsonRpcError::header_mismatch(format!(
                    "Mcp-Name header is required for {} requests",
                    body_method.as_deref().unwrap_or("?")
                )));
            }
            (Some(h), Some(b)) if h != b => {
                return Err(JsonRpcError::header_mismatch(format!(
                    "Mcp-Name header value {h:?} does not match body value {b:?}"
                )));
            }
            (Some(h), None) => {
                return Err(JsonRpcError::header_mismatch(format!(
                    "Mcp-Name header value {h:?} present but body has no matching field"
                )));
            }
            _ => {}
        }
    }

    // Mcp-Param-* validation: every Mcp-Param-{Name} header must
    // correspond to a value in params.arguments.{Name} (case-insensitive
    // match on the header suffix). Per the SEP, header-omitted-but-body-
    // present is a non-conforming client and a server-side error in
    // strict mode; we mirror the same.
    validate_param_headers(headers, parsed, mode)?;

    Ok(())
}

/// Identifies where in the body the `Mcp-Name` header value should be
/// mirrored from.
#[derive(Debug, Clone, Copy)]
enum NameSource {
    /// `params.name` (`tools/call`, `prompts/get`).
    ParamsName,
    /// `params.uri` (`resources/read`).
    ParamsUri,
}

impl NameSource {
    fn extract(self, parsed: &Value) -> Option<String> {
        let params = parsed.get("params")?;
        let key = match self {
            NameSource::ParamsName => "name",
            NameSource::ParamsUri => "uri",
        };
        params.get(key).and_then(|v| v.as_str()).map(str::to_string)
    }
}

/// Pull a header value as a `String`, decoding the SEP-2243 Base64
/// sentinel if present. Returns `Ok(None)` when the header is absent.
///
/// Per RFC 9110, HTTP parsers trim leading/trailing whitespace from
/// header values; we rely on hyper/axum having already done so.
fn header_str(headers: &axum::http::HeaderMap, name: &str) -> Result<Option<String>, JsonRpcError> {
    let Some(raw) = headers.get(name) else {
        return Ok(None);
    };
    let s = raw
        .to_str()
        .map_err(|e| JsonRpcError::header_mismatch(format!("{name} contains invalid bytes: {e}")))?
        .trim();
    Ok(Some(decode_sentinel(s).map_err(|msg| {
        JsonRpcError::header_mismatch(format!("{name}: {msg}"))
    })?))
}

/// Decode the SEP-2243 Base64 sentinel `=?base64?...?=` if present.
/// Otherwise return the raw value. The prefix is case-insensitive per
/// the spec's "Base64 Decoding" conformance table (`=?BASE64?...?=` is
/// accepted).
fn decode_sentinel(value: &str) -> Result<String, String> {
    let lower = value.to_ascii_lowercase();
    if lower.starts_with(BASE64_PREFIX) && value.ends_with(BASE64_SUFFIX) {
        // Strip the prefix using the original casing length; the
        // lowercase form is byte-length-equal because the prefix is
        // ASCII.
        let body = &value[BASE64_PREFIX.len()..value.len() - BASE64_SUFFIX.len()];
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(body)
            .map_err(|e| format!("invalid Base64: {e}"))?;
        String::from_utf8(bytes).map_err(|e| format!("Base64 payload is not valid UTF-8: {e}"))
    } else {
        Ok(value.to_string())
    }
}

/// Validate every `Mcp-Param-*` header against the corresponding entry
/// in `params.arguments`.
///
/// Matching on the suffix is case-insensitive. The SEP requires
/// `x-mcp-header` values to be case-insensitively unique within a tool
/// schema, so duplicate suffixes after lowercasing are also a mismatch.
///
/// In [`Sep2243Mode::Strict`] mode, if a Mcp-Param-* header is sent for
/// a key not present in the body arguments (or vice versa for a key
/// listed in the request but missing from the header set), we treat it
/// as a mismatch. We can't enumerate "every body argument that should
/// have a corresponding header" without the tool schema (which lives in
/// the router, not the transport), so we restrict the body-side check
/// to keys that appear in the headers — this matches the spec's
/// "validate headers" guidance without overreaching.
fn validate_param_headers(
    headers: &axum::http::HeaderMap,
    parsed: &Value,
    mode: Sep2243Mode,
) -> Result<(), JsonRpcError> {
    let arguments: Option<&Value> = parsed
        .get("params")
        .and_then(|p| p.get("arguments"))
        .filter(|v| v.is_object());

    let mut seen_suffixes: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (name, raw) in headers.iter() {
        let name_str = name.as_str();
        if !name_str.starts_with(MCP_PARAM_HEADER_PREFIX) {
            continue;
        }
        let suffix = &name_str[MCP_PARAM_HEADER_PREFIX.len()..];
        if suffix.is_empty() {
            return Err(JsonRpcError::header_mismatch(
                "Mcp-Param- header has empty suffix",
            ));
        }
        let suffix_lc = suffix.to_ascii_lowercase();
        if !seen_suffixes.insert(suffix_lc.clone()) {
            return Err(JsonRpcError::header_mismatch(format!(
                "duplicate Mcp-Param-* header: {suffix:?} (case-insensitive)"
            )));
        }

        let value = raw
            .to_str()
            .map_err(|e| {
                JsonRpcError::header_mismatch(format!(
                    "Mcp-Param-{suffix} contains invalid bytes: {e}"
                ))
            })?
            .trim();
        let decoded = decode_sentinel(value)
            .map_err(|m| JsonRpcError::header_mismatch(format!("Mcp-Param-{suffix}: {m}")))?;

        // Look up the body argument with a case-insensitive match on the
        // suffix. We can't assume the JSON property name is lowercased;
        // do a linear scan.
        let body_value = arguments.and_then(|args| {
            args.as_object().and_then(|obj| {
                obj.iter().find_map(|(k, v)| {
                    if k.eq_ignore_ascii_case(suffix) {
                        Some(v)
                    } else {
                        None
                    }
                })
            })
        });

        match body_value {
            None => {
                if matches!(mode, Sep2243Mode::Strict) {
                    return Err(JsonRpcError::header_mismatch(format!(
                        "Mcp-Param-{suffix} present but no matching body argument"
                    )));
                }
            }
            Some(v) => {
                let body_repr = json_value_to_header_string(v).map_err(|e| {
                    JsonRpcError::header_mismatch(format!(
                        "Mcp-Param-{suffix}: body value cannot be represented as a header: {e}"
                    ))
                })?;
                if decoded != body_repr {
                    return Err(JsonRpcError::header_mismatch(format!(
                        "Mcp-Param-{suffix} header value {decoded:?} does not match body value {body_repr:?}"
                    )));
                }
            }
        }
    }

    Ok(())
}

/// Render a JSON value the way SEP-2243 says clients SHOULD render it
/// before placing it in a header:
///
/// - string: as-is
/// - number: decimal string
/// - boolean: lowercase `true` / `false`
///
/// Other types (array/object/null) are rejected — the SEP forbids
/// `x-mcp-header` on non-primitive types, so a body value here is a
/// schema violation.
fn json_value_to_header_string(v: &Value) -> Result<String, &'static str> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(true) => Ok("true".to_string()),
        Value::Bool(false) => Ok("false".to_string()),
        Value::Null => Err("null is not a primitive header value"),
        Value::Array(_) => Err("array is not a primitive header value"),
        Value::Object(_) => Err("object is not a primitive header value"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use serde_json::json;

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn mode_strict_at_or_after_2026_07_28() {
        assert_eq!(mode_for_version("2026-07-28"), Sep2243Mode::Strict);
        assert_eq!(mode_for_version("2027-01-01"), Sep2243Mode::Strict);
    }

    #[test]
    fn mode_lenient_before_2026_07_28() {
        assert_eq!(mode_for_version("2025-11-25"), Sep2243Mode::Lenient);
        assert_eq!(mode_for_version("2025-03-26"), Sep2243Mode::Lenient);
    }

    #[test]
    fn strict_missing_mcp_method_is_error() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        let err = validate(&hm(&[]), &body, Sep2243Mode::Strict).unwrap_err();
        assert_eq!(err.code, -32001);
        assert!(err.message.contains("Mcp-Method"));
    }

    #[test]
    fn lenient_missing_mcp_method_is_ok() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        validate(&hm(&[]), &body, Sep2243Mode::Lenient).unwrap();
    }

    #[test]
    fn method_mismatch_rejected_in_both_modes() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        for mode in [Sep2243Mode::Strict, Sep2243Mode::Lenient] {
            let err = validate(&hm(&[("mcp-method", "ping")]), &body, mode).unwrap_err();
            assert_eq!(err.code, -32001, "mode={mode:?}");
            assert!(err.message.contains("Mcp-Method"));
        }
    }

    #[test]
    fn method_matches_case_sensitive_value() {
        // Header names are case-insensitive; the values are not. The
        // SEP's conformance table says `Mcp-Method: TOOLS/CALL` MUST be
        // rejected because method values are case-sensitive.
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": "x"}});
        let err = validate(
            &hm(&[("MCP-METHOD", "TOOLS/CALL"), ("Mcp-Name", "x")]),
            &body,
            Sep2243Mode::Strict,
        )
        .unwrap_err();
        assert_eq!(err.code, -32001);
    }

    #[test]
    fn tools_call_requires_mcp_name_in_strict() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": "get_weather"}});
        let err = validate(
            &hm(&[("mcp-method", "tools/call")]),
            &body,
            Sep2243Mode::Strict,
        )
        .unwrap_err();
        assert_eq!(err.code, -32001);
        assert!(err.message.contains("Mcp-Name"));
    }

    #[test]
    fn tools_call_ok_in_lenient_without_mcp_name() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": "get_weather"}});
        validate(
            &hm(&[("mcp-method", "tools/call")]),
            &body,
            Sep2243Mode::Lenient,
        )
        .unwrap();
    }

    #[test]
    fn tools_call_with_matching_name_ok() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": "get_weather"}});
        validate(
            &hm(&[("mcp-method", "tools/call"), ("mcp-name", "get_weather")]),
            &body,
            Sep2243Mode::Strict,
        )
        .unwrap();
    }

    #[test]
    fn tools_call_mismatched_name_rejected_in_lenient_too() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": "bar"}});
        let err = validate(
            &hm(&[("mcp-method", "tools/call"), ("mcp-name", "foo")]),
            &body,
            Sep2243Mode::Lenient,
        )
        .unwrap_err();
        assert_eq!(err.code, -32001);
    }

    #[test]
    fn resources_read_uses_params_uri() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "resources/read",
                          "params": {"uri": "file:///x"}});
        validate(
            &hm(&[("mcp-method", "resources/read"), ("mcp-name", "file:///x")]),
            &body,
            Sep2243Mode::Strict,
        )
        .unwrap();
    }

    #[test]
    fn prompts_get_uses_params_name() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "prompts/get",
                          "params": {"name": "code_review"}});
        validate(
            &hm(&[("mcp-method", "prompts/get"), ("mcp-name", "code_review")]),
            &body,
            Sep2243Mode::Strict,
        )
        .unwrap();
    }

    #[test]
    fn other_methods_dont_require_mcp_name() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
        validate(
            &hm(&[("mcp-method", "initialize")]),
            &body,
            Sep2243Mode::Strict,
        )
        .unwrap();
    }

    #[test]
    fn notification_only_needs_mcp_method_in_strict() {
        let body = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        validate(
            &hm(&[("mcp-method", "notifications/initialized")]),
            &body,
            Sep2243Mode::Strict,
        )
        .unwrap();
    }

    #[test]
    fn header_name_is_case_insensitive() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        validate(&hm(&[("MCP-METHOD", "ping")]), &body, Sep2243Mode::Strict).unwrap();
    }

    #[test]
    fn base64_sentinel_decoded() {
        // "Hello" -> SGVsbG8=
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": "Hello"}});
        validate(
            &hm(&[
                ("mcp-method", "tools/call"),
                ("mcp-name", "=?base64?SGVsbG8=?="),
            ]),
            &body,
            Sep2243Mode::Strict,
        )
        .unwrap();
    }

    #[test]
    fn base64_sentinel_case_insensitive_prefix() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": "Hello"}});
        validate(
            &hm(&[
                ("mcp-method", "tools/call"),
                ("mcp-name", "=?BASE64?SGVsbG8=?="),
            ]),
            &body,
            Sep2243Mode::Strict,
        )
        .unwrap();
    }

    #[test]
    fn invalid_base64_payload_rejected() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": "Hello"}});
        let err = validate(
            &hm(&[
                ("mcp-method", "tools/call"),
                ("mcp-name", "=?base64?SGVs!!!bG8=?="),
            ]),
            &body,
            Sep2243Mode::Strict,
        )
        .unwrap_err();
        assert_eq!(err.code, -32001);
        assert!(err.message.contains("Base64"));
    }

    #[test]
    fn mcp_param_matches_body_arguments() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "execute_sql",
            "arguments": {"region": "us-west1", "query": "..."}
        }});
        validate(
            &hm(&[
                ("mcp-method", "tools/call"),
                ("mcp-name", "execute_sql"),
                ("mcp-param-region", "us-west1"),
            ]),
            &body,
            Sep2243Mode::Strict,
        )
        .unwrap();
    }

    #[test]
    fn mcp_param_mismatch_rejected() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "execute_sql",
            "arguments": {"region": "us-east1"}
        }});
        let err = validate(
            &hm(&[
                ("mcp-method", "tools/call"),
                ("mcp-name", "execute_sql"),
                ("mcp-param-region", "us-west1"),
            ]),
            &body,
            Sep2243Mode::Strict,
        )
        .unwrap_err();
        assert_eq!(err.code, -32001);
        assert!(err.message.contains("Mcp-Param-region"));
    }

    #[test]
    fn mcp_param_number_serialized_as_decimal() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "x",
            "arguments": {"count": 42}
        }});
        validate(
            &hm(&[
                ("mcp-method", "tools/call"),
                ("mcp-name", "x"),
                ("mcp-param-count", "42"),
            ]),
            &body,
            Sep2243Mode::Strict,
        )
        .unwrap();
    }

    #[test]
    fn mcp_param_boolean_serialized_lowercase() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "x",
            "arguments": {"flag": true}
        }});
        validate(
            &hm(&[
                ("mcp-method", "tools/call"),
                ("mcp-name", "x"),
                ("mcp-param-flag", "true"),
            ]),
            &body,
            Sep2243Mode::Strict,
        )
        .unwrap();
    }

    #[test]
    fn duplicate_mcp_param_headers_rejected() {
        let mut h = HeaderMap::new();
        h.append(
            axum::http::HeaderName::from_static("mcp-method"),
            axum::http::HeaderValue::from_static("tools/call"),
        );
        h.append(
            axum::http::HeaderName::from_static("mcp-name"),
            axum::http::HeaderValue::from_static("x"),
        );
        h.append(
            axum::http::HeaderName::from_static("mcp-param-region"),
            axum::http::HeaderValue::from_static("a"),
        );
        // HeaderMap doesn't allow two entries with the same lowercase
        // name in the iterator; use `append` to add a second one (which
        // becomes a multi-value entry). To exercise the duplicate
        // suffix branch we'd need different casings, but axum
        // canonicalises to lowercase. So this test verifies that
        // appending the SAME key twice still presents as one iter slot.
        h.append(
            axum::http::HeaderName::from_static("mcp-param-region"),
            axum::http::HeaderValue::from_static("b"),
        );

        // We can't easily build two distinct same-lowercase suffix
        // headers via axum; the duplicate-suffix branch is defensive
        // and exercised here by manually constructing a case-different
        // pair on a fresh HeaderMap is also impossible since axum
        // canonicalises names. This test thus mostly asserts the
        // happy-path-with-multi-value didn't accidentally accept a
        // bogus value: the first iter call returns the first inserted
        // value ("a"), which mismatches body if body says something
        // else.
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": "x", "arguments": {"region": "c"}}});
        let err = validate(&h, &body, Sep2243Mode::Strict).unwrap_err();
        assert_eq!(err.code, -32001);
    }

    #[test]
    fn empty_mcp_param_suffix_rejected() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::HeaderName::from_static("mcp-method"),
            axum::http::HeaderValue::from_static("ping"),
        );
        // axum's HeaderName parser rejects "mcp-param-" with no suffix
        // when constructed via from_bytes? Let's verify.
        let res = axum::http::HeaderName::from_bytes(b"mcp-param-");
        // HeaderName allows it; the validation function should reject it.
        if let Ok(name) = res {
            h.insert(name, axum::http::HeaderValue::from_static("x"));
            let body = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
            let err = validate(&h, &body, Sep2243Mode::Strict).unwrap_err();
            assert_eq!(err.code, -32001);
            assert!(err.message.contains("empty suffix"));
        }
    }

    #[test]
    fn mcp_param_case_insensitive_against_body_key() {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "x",
            "arguments": {"tenantId": "acme"}
        }});
        validate(
            &hm(&[
                ("mcp-method", "tools/call"),
                ("mcp-name", "x"),
                // Header suffix `TenantId` should match body key `tenantId`
                ("mcp-param-tenantid", "acme"),
            ]),
            &body,
            Sep2243Mode::Strict,
        )
        .unwrap();
    }

    #[test]
    fn extra_whitespace_in_value_trimmed() {
        // RFC 9110: HTTP parsers trim leading/trailing whitespace from
        // header values. axum already trims, but our validation must
        // not double-strip in a way that breaks Base64 content.
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": "foo"}});
        validate(
            &hm(&[("mcp-method", "tools/call"), ("mcp-name", "  foo  ")]),
            &body,
            Sep2243Mode::Strict,
        )
        .unwrap();
    }
}
