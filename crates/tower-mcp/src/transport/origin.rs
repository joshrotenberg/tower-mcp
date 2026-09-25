//! `Origin` validation shared by the HTTP and WebSocket transports.
//!
//! Browsers attach `Origin` to cross-site `fetch` requests and to every
//! WebSocket handshake, and they do not apply CORS to WebSocket connections.
//! Checking it on the server is what keeps an arbitrary web page from driving
//! a locally bound MCP server (#1464).

use std::net::IpAddr;

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

pub(crate) fn is_localhost_origin(origin: &str) -> bool {
    // Parse the origin to extract the host. RFC 3986 makes the URI scheme
    // case-insensitive, so the prefix match must be too.
    strip_scheme_ci(origin, "http://")
        .or_else(|| strip_scheme_ci(origin, "https://"))
        .is_some_and(is_localhost_host)
}

/// Case-insensitively strip an ASCII scheme prefix (e.g. `"http://"`) from
/// `s`, returning the remainder if `s` starts with it.
fn strip_scheme_ci<'a>(s: &'a str, scheme: &str) -> Option<&'a str> {
    let prefix = scheme.as_bytes();
    if s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Check if a `host:port` (or `[ipv6]:port`) value refers to localhost.
///
/// Used by both Origin validation (after stripping the `http(s)://` scheme)
/// and Host validation (where there's no scheme to begin with).
pub(crate) fn is_localhost_host(host: &str) -> bool {
    // A bare (unbracketed, port-less) IPv6 literal like `::1` parses whole
    // as an IpAddr. Check this first: the port-splitting logic below would
    // otherwise misread one of its internal colons as a port separator.
    // A bare IPv6 literal with a trailing segment (e.g. `::1:3000`) parses
    // whole as a distinct, non-loopback address rather than `::1` plus a
    // port -- RFC 3986 requires brackets around an IPv6 host whenever a
    // port follows it, so that's the correct outcome, not a special case.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return ip.is_loopback();
    }

    let host_only = if host.starts_with('[') {
        // Bracketed IPv6: [::1]:3000 -> ::1. RFC 3986 permits nothing
        // after the closing bracket but an optional ":port"; a missing
        // closing bracket, or any other trailing content, makes the
        // authority invalid and must be rejected rather than silently
        // discarded (that was the bug: [::1]evil.com used to be read as
        // just [::1]).
        let Some(close) = host.find(']') else {
            return false;
        };
        let after_bracket = &host[close + 1..];
        let port_ok = after_bracket.is_empty()
            || after_bracket
                .strip_prefix(':')
                .is_some_and(|port| !port.is_empty() && port.parse::<u16>().is_ok());
        if !port_ok {
            return false;
        }
        &host[1..close]
    } else {
        // Strip port if present
        host.split(':').next().unwrap_or(host)
    };

    // `localhost`, and its RFC 3986 trailing-dot FQDN form, is
    // case-insensitive like any DNS name.
    if host_only.eq_ignore_ascii_case("localhost") || host_only.eq_ignore_ascii_case("localhost.") {
        return true;
    }

    // Covers the entire 127.0.0.0/8 range and ::1 (the only IPv6 loopback
    // address) in one shot. Rust's std parser requires canonical
    // dotted-decimal IPv4 (rejecting shorthand and hex/octal/decimal
    // obfuscation forms), so this doesn't widen the guard beyond the
    // canonical numeric forms a conforming URL host parser would produce.
    host_only.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Validate the `Origin` header.
///
/// When `enabled`:
/// - Requests without an Origin header are allowed (same-origin, or a
///   non-browser client)
/// - Localhost origins are always allowed (DNS rebinding protection)
/// - If `allowed_origins` is non-empty, non-localhost origins must match
/// - If `allowed_origins` is empty, non-localhost origins are rejected
///
/// Returns `Some(403 response)` if validation fails, `None` if it passes.
pub(crate) fn validate_origin(
    headers: &HeaderMap,
    enabled: bool,
    allowed_origins: &[String],
) -> Option<Response> {
    if !enabled {
        return None;
    }

    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin_str = origin.to_str().unwrap_or("");

        // Always allow localhost origins (DNS rebinding protection allows these)
        if is_localhost_origin(origin_str) {
            return None;
        }

        // Non-localhost origin: check against allowed list
        if allowed_origins.is_empty() {
            tracing::warn!(
                origin = %origin_str,
                "Rejecting request: cross-origin not allowed (no allowlist configured)"
            );
            return Some(
                (StatusCode::FORBIDDEN, "Cross-origin requests not allowed").into_response(),
            );
        }

        if !allowed_origins.iter().any(|o| o == origin_str || o == "*") {
            tracing::warn!(origin = %origin_str, "Rejecting request: Origin not in allowlist");
            return Some((StatusCode::FORBIDDEN, "Origin not allowed").into_response());
        }
    }

    None
}
