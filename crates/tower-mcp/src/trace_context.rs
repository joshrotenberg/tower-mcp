//! W3C Trace Context carried in request `_meta` (SEP-414).
//!
//! SEP-414 reserves the `traceparent` and `tracestate` keys in a request's
//! `_meta` object for [W3C Trace Context]. A client that is already inside a
//! trace can send them so the server's work joins that trace.
//!
//! [`JsonRpcService`](crate::jsonrpc::JsonRpcService) reads both keys from
//! `params._meta` on every request, on the 2025-11-25 lifecycle and on
//! 2026-07-28, and attaches a [`TraceContext`] to the request extensions.
//! Handlers read it with
//! [`RequestContext::trace_context`](crate::context::RequestContext::trace_context),
//! and [`McpTracingLayer`](crate::middleware::McpTracingLayer) records its
//! identifiers on the `mcp_request` span.
//!
//! ```rust
//! use tower_mcp::TraceContext;
//!
//! let ctx = TraceContext::parse(
//!     "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
//!     Some("vendor=value"),
//! )
//! .expect("valid traceparent");
//! assert_eq!(ctx.trace_id(), "4bf92f3577b34da6a3ce929d0e0e4736");
//! assert_eq!(ctx.parent_id(), "00f067aa0ba902b7");
//! assert!(ctx.sampled());
//! assert_eq!(ctx.tracestate(), Some("vendor=value"));
//! ```
//!
//! This crate reads the context and exposes it. It does not link the
//! `tracing` span to an OpenTelemetry parent.
//!
//! # Parsing rules
//!
//! - `traceparent` is `version-trace-id-parent-id-trace-flags`, all lowercase
//!   hex: a 2-digit version, a 32-digit trace id, a 16-digit parent id, and a
//!   2-digit flags field. The trace id and parent id must not be all zeros.
//! - Version `00` must be exactly 55 characters, with no trailing fields.
//! - Version `ff` is invalid.
//! - Any other version is read with the W3C forward-compatibility rules: the
//!   first four fields must satisfy the same rules as version `00`, and the
//!   value must either end there or continue with `-` and further fields. The
//!   further fields are ignored.
//! - `tracestate` is kept as the raw string. It is dropped, and the
//!   `traceparent` kept, when it is longer than 512 characters or has more
//!   than 32 list members. Its member syntax is not validated. An empty
//!   `tracestate` reads as absent.
//! - A malformed `traceparent` discards both keys and emits a debug-level
//!   `tracing` event. The request itself is unaffected.
//!
//! [W3C Trace Context]: https://www.w3.org/TR/trace-context/

use serde_json::Value;

/// Length of a version `00` `traceparent`: `2 + 1 + 32 + 1 + 16 + 1 + 2`.
const TRACEPARENT_LEN: usize = 55;
/// Maximum accepted `tracestate` length, in characters.
const MAX_TRACESTATE_LEN: usize = 512;
/// Maximum accepted number of `tracestate` list members.
const MAX_TRACESTATE_MEMBERS: usize = 32;

/// The W3C Trace Context a client sent in a request's `_meta`.
///
/// Obtain one from
/// [`RequestContext::trace_context`](crate::context::RequestContext::trace_context),
/// or parse a value directly with [`TraceContext::parse`]. See the
/// [module documentation](self) for the parsing rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceContext {
    version: u8,
    trace_id: String,
    parent_id: String,
    trace_flags: u8,
    tracestate: Option<String>,
}

impl TraceContext {
    /// Parse a `traceparent` value and an optional `tracestate` value.
    ///
    /// Returns `None` when `traceparent` is malformed. An invalid
    /// `tracestate` is dropped and the `traceparent` is kept.
    pub fn parse(traceparent: &str, tracestate: Option<&str>) -> Option<Self> {
        let (version, trace_id, parent_id, trace_flags) = parse_traceparent(traceparent)?;
        Some(Self {
            version,
            trace_id: trace_id.to_string(),
            parent_id: parent_id.to_string(),
            trace_flags,
            tracestate: tracestate.and_then(valid_tracestate).map(str::to_string),
        })
    }

    /// Read the trace context from a request's `_meta` object.
    ///
    /// Returns `None` when `meta` is not an object or has no `traceparent`.
    /// A `traceparent` that is present but not a well-formed string discards
    /// both keys and emits a debug-level event. A `tracestate` that is not a
    /// string reads as absent.
    pub fn from_meta(meta: &Value) -> Option<Self> {
        let meta = meta.as_object()?;
        let traceparent = meta.get("traceparent")?;
        let Some(traceparent) = traceparent.as_str() else {
            tracing::debug!("ignoring trace context: traceparent is not a string");
            return None;
        };
        let tracestate = meta.get("tracestate").and_then(Value::as_str);
        let parsed = Self::parse(traceparent, tracestate);
        if parsed.is_none() {
            tracing::debug!(
                traceparent_len = traceparent.len(),
                "ignoring trace context: malformed traceparent"
            );
        }
        parsed
    }

    /// The `traceparent` version, `00` for every value defined today.
    pub fn version(&self) -> u8 {
        self.version
    }

    /// The trace id: 32 lowercase hex digits, never all zeros.
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    /// The caller's span id (`parent-id`): 16 lowercase hex digits, never all
    /// zeros. The server's own span is a child of this one.
    pub fn parent_id(&self) -> &str {
        &self.parent_id
    }

    /// The raw `trace-flags` byte.
    pub fn trace_flags(&self) -> u8 {
        self.trace_flags
    }

    /// Whether the caller set the `sampled` flag (bit 0 of the flags).
    pub fn sampled(&self) -> bool {
        self.trace_flags & 0x01 != 0
    }

    /// The raw `tracestate` string, if one was sent and it passed the length
    /// and member-count limits.
    pub fn tracestate(&self) -> Option<&str> {
        self.tracestate.as_deref()
    }
}

/// Split and validate a `traceparent`, returning the version, trace id, parent
/// id, and flags. Returns `None` for anything the module rules reject.
fn parse_traceparent(value: &str) -> Option<(u8, &str, &str, u8)> {
    // Every byte of a valid value is ASCII, so byte indexing below cannot
    // split a character once this holds.
    if !value.is_ascii() || value.len() < TRACEPARENT_LEN {
        return None;
    }
    let bytes = value.as_bytes();
    if bytes[2] != b'-' || bytes[35] != b'-' || bytes[52] != b'-' {
        return None;
    }

    let version = value.get(0..2)?;
    let trace_id = value.get(3..35)?;
    let parent_id = value.get(36..52)?;
    let flags = value.get(53..55)?;
    if !is_lower_hex(version)
        || !is_lower_hex(trace_id)
        || !is_lower_hex(parent_id)
        || !is_lower_hex(flags)
    {
        return None;
    }
    if is_all_zero(trace_id) || is_all_zero(parent_id) {
        return None;
    }

    let version = u8::from_str_radix(version, 16).ok()?;
    match version {
        0xff => return None,
        // Version 00 has exactly four fields.
        0x00 if value.len() != TRACEPARENT_LEN => return None,
        // A later version may append fields, each introduced by a dash.
        _ if value.len() > TRACEPARENT_LEN && bytes[TRACEPARENT_LEN] != b'-' => return None,
        _ => {}
    }

    let flags = u8::from_str_radix(flags, 16).ok()?;
    Some((version, trace_id, parent_id, flags))
}

/// Return `value` when it is an acceptable `tracestate`.
fn valid_tracestate(value: &str) -> Option<&str> {
    if value.chars().count() > MAX_TRACESTATE_LEN {
        return None;
    }
    let members = value
        .split(',')
        .filter(|member| !member.trim().is_empty())
        .count();
    if members == 0 || members > MAX_TRACESTATE_MEMBERS {
        return None;
    }
    Some(value)
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_all_zero(value: &str) -> bool {
    value.bytes().all(|byte| byte == b'0')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    const PARENT_ID: &str = "00f067aa0ba902b7";
    const VALID: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn parses_a_valid_traceparent_without_tracestate() {
        let ctx = TraceContext::parse(VALID, None).unwrap();
        assert_eq!(ctx.version(), 0);
        assert_eq!(ctx.trace_id(), TRACE_ID);
        assert_eq!(ctx.parent_id(), PARENT_ID);
        assert_eq!(ctx.trace_flags(), 1);
        assert!(ctx.sampled());
        assert_eq!(ctx.tracestate(), None);
    }

    #[test]
    fn parses_a_valid_traceparent_with_tracestate() {
        let ctx =
            TraceContext::parse(VALID, Some("congo=t61rcWkgMzE,rojo=00f067aa0ba902b7")).unwrap();
        assert_eq!(
            ctx.tracestate(),
            Some("congo=t61rcWkgMzE,rojo=00f067aa0ba902b7")
        );
    }

    #[test]
    fn unsampled_flag_is_reported() {
        let ctx = TraceContext::parse(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00",
            None,
        )
        .unwrap();
        assert!(!ctx.sampled());
        assert_eq!(ctx.trace_flags(), 0);
    }

    #[test]
    fn sampled_is_bit_zero_only() {
        let ctx = TraceContext::parse(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-02",
            None,
        )
        .unwrap();
        assert!(!ctx.sampled());
        assert_eq!(ctx.trace_flags(), 2);
    }

    #[test]
    fn rejects_wrong_length() {
        for value in [
            "",
            "00",
            &VALID[..54],
            &format!("{VALID}0"),
            // Version 00 allows no trailing fields, even dash-separated ones.
            &format!("{VALID}-extra"),
            "00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e47366-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-1",
        ] {
            assert!(TraceContext::parse(value, None).is_none(), "{value:?}");
        }
    }

    #[test]
    fn rejects_uppercase_hex() {
        for value in [
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00F067AA0BA902B7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-0A",
            "0A-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        ] {
            assert!(TraceContext::parse(value, None).is_none(), "{value:?}");
        }
    }

    #[test]
    fn rejects_non_hex_and_bad_separators() {
        for value in [
            "00-4bf92f3577b34da6a3ce929d0e0e473g-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902bz-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-0x",
            "zz-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00_4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736_00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7_01",
            // Multibyte input must be rejected without panicking.
            "00-4bf92f3577b34da6a3ce929d0e0e473\u{e9}-00f067aa0ba902b7-01",
        ] {
            assert!(TraceContext::parse(value, None).is_none(), "{value:?}");
        }
    }

    #[test]
    fn rejects_all_zero_trace_id() {
        let value = "00-00000000000000000000000000000000-00f067aa0ba902b7-01";
        assert!(TraceContext::parse(value, None).is_none());
    }

    #[test]
    fn rejects_all_zero_parent_id() {
        let value = "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01";
        assert!(TraceContext::parse(value, None).is_none());
    }

    #[test]
    fn rejects_version_ff() {
        let value = "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert!(TraceContext::parse(value, None).is_none());
    }

    #[test]
    fn future_version_reads_the_first_four_fields() {
        // Exactly four fields.
        let ctx = TraceContext::parse(
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            None,
        )
        .unwrap();
        assert_eq!(ctx.version(), 1);
        assert_eq!(ctx.trace_id(), TRACE_ID);
        assert_eq!(ctx.parent_id(), PARENT_ID);
        assert!(ctx.sampled());

        // Unknown trailing fields are ignored.
        let ctx = TraceContext::parse(
            "cc-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-what-the-future-will-be-like",
            None,
        )
        .unwrap();
        assert_eq!(ctx.version(), 0xcc);
        assert_eq!(ctx.trace_id(), TRACE_ID);
    }

    #[test]
    fn future_version_still_enforces_the_field_rules() {
        for value in [
            // Trailing data must start with a dash.
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01x",
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-010",
            // Too short to hold four fields.
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "01-00000000000000000000000000000000-00f067aa0ba902b7-01-x",
            "01-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01-x",
            "01-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01-x",
        ] {
            assert!(TraceContext::parse(value, None).is_none(), "{value:?}");
        }
    }

    #[test]
    fn oversized_tracestate_is_dropped_but_traceparent_kept() {
        let too_long = format!("k={}", "v".repeat(MAX_TRACESTATE_LEN));
        let ctx = TraceContext::parse(VALID, Some(&too_long)).unwrap();
        assert_eq!(ctx.trace_id(), TRACE_ID);
        assert_eq!(ctx.tracestate(), None);

        let at_limit = format!("k={}", "v".repeat(MAX_TRACESTATE_LEN - 2));
        assert_eq!(at_limit.len(), MAX_TRACESTATE_LEN);
        let ctx = TraceContext::parse(VALID, Some(&at_limit)).unwrap();
        assert_eq!(ctx.tracestate(), Some(at_limit.as_str()));
    }

    #[test]
    fn tracestate_over_the_member_limit_is_dropped() {
        let members = |n: usize| {
            (0..n)
                .map(|i| format!("k{i}=v"))
                .collect::<Vec<_>>()
                .join(",")
        };
        let ok = members(MAX_TRACESTATE_MEMBERS);
        let ctx = TraceContext::parse(VALID, Some(&ok)).unwrap();
        assert_eq!(ctx.tracestate(), Some(ok.as_str()));

        let too_many = members(MAX_TRACESTATE_MEMBERS + 1);
        let ctx = TraceContext::parse(VALID, Some(&too_many)).unwrap();
        assert_eq!(ctx.trace_id(), TRACE_ID);
        assert_eq!(ctx.tracestate(), None);
    }

    #[test]
    fn empty_tracestate_reads_as_absent() {
        for value in ["", " ", ",", " , "] {
            let ctx = TraceContext::parse(VALID, Some(value)).unwrap();
            assert_eq!(ctx.tracestate(), None, "{value:?}");
        }
    }

    #[test]
    fn from_meta_reads_both_keys() {
        let meta = json!({
            "traceparent": VALID,
            "tracestate": "vendor=value",
            "progressToken": "p",
        });
        let ctx = TraceContext::from_meta(&meta).unwrap();
        assert_eq!(ctx.trace_id(), TRACE_ID);
        assert_eq!(ctx.tracestate(), Some("vendor=value"));
    }

    #[test]
    fn from_meta_ignores_absent_and_malformed_values() {
        for meta in [
            json!(null),
            json!("traceparent"),
            json!({}),
            json!({"tracestate": "vendor=value"}),
            json!({"traceparent": 7}),
            json!({"traceparent": null}),
            json!({"traceparent": "not-a-traceparent", "tracestate": "vendor=value"}),
        ] {
            assert!(TraceContext::from_meta(&meta).is_none(), "{meta}");
        }
    }

    #[test]
    fn from_meta_treats_a_non_string_tracestate_as_absent() {
        let meta = json!({"traceparent": VALID, "tracestate": 5});
        let ctx = TraceContext::from_meta(&meta).unwrap();
        assert_eq!(ctx.trace_id(), TRACE_ID);
        assert_eq!(ctx.tracestate(), None);
    }
}
