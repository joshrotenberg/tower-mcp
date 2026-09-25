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

/// A single `allowed_origins` entry, parsed and normalized once when the
/// transport is built.
///
/// Comparing raw strings (the previous behavior) makes an allowlist entry
/// that differs from a browser's exact serialization silently ineffective:
/// `https://example.com:443` never matches `https://example.com`, and
/// likewise for a different-case host or a trailing slash (#1476). Storing
/// the parsed, normalized form instead makes those variants compare equal
/// and does the parsing once at build time rather than on every request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AllowedOrigin {
    /// `"*"`: matches any Origin.
    Any,
    /// A specific origin: scheme (`"http"` or `"https"` after normalization,
    /// see [`parse_origin`]), lowercased host, and the port with the
    /// scheme's default applied when the entry didn't specify one.
    Origin {
        scheme: &'static str,
        host: String,
        port: u16,
    },
}

/// Parse and normalize a single `Origin` value: either an `allowed_origins`
/// allowlist entry, or an incoming `Origin` header, since both need the same
/// (scheme, host, port) comparison key.
///
/// Rejects anything that is not a bare `scheme://host[:port]` origin -- a
/// path other than `/`, a query, a fragment, userinfo, or a scheme other
/// than `http`/`https`/`ws`/`wss`. `ws` and `wss` are accepted and mapped to
/// `http` and `https` respectively: a browser's `Origin` header is always
/// `http(s)`, even on a WebSocket upgrade (the header reflects the scheme of
/// the page that opened the connection, not the connection's own scheme), so
/// an incoming Origin never actually arrives as `ws://`/`wss://`. Accepting
/// those schemes here is purely a convenience for an operator writing
/// `allowed_origins(vec!["wss://api.example.com"])` for a WebSocket-only
/// server, who is describing the connection scheme rather than the
/// browser-visible one; normalizing it to `https` is what makes it actually
/// match.
fn parse_origin(value: &str) -> Result<(&'static str, String, u16), String> {
    // `Origin` never carries a fragment, but `http::Uri` silently discards
    // one instead of reporting it, so it has to be rejected before parsing
    // rather than after.
    if value.contains('#') {
        return Err(format!(
            "{value:?} has a fragment, which an Origin cannot carry"
        ));
    }

    let uri: axum::http::Uri = value
        .parse()
        .map_err(|e| format!("{value:?} is not a valid Origin: {e}"))?;

    let scheme = uri
        .scheme_str()
        .ok_or_else(|| format!("{value:?} has no scheme"))?;
    let (scheme, default_port) = match scheme.to_ascii_lowercase().as_str() {
        "http" => ("http", 80u16),
        "https" => ("https", 443u16),
        "ws" => ("http", 80u16),
        "wss" => ("https", 443u16),
        other => return Err(format!("{value:?} has unsupported scheme {other:?}")),
    };

    let authority = uri
        .authority()
        .ok_or_else(|| format!("{value:?} has no host"))?;
    if authority.as_str().contains('@') {
        return Err(format!("{value:?} must not contain userinfo"));
    }

    let path = uri.path();
    if !path.is_empty() && path != "/" {
        return Err(format!("{value:?} must not have a path"));
    }
    if uri.query().is_some() {
        return Err(format!("{value:?} must not have a query"));
    }

    Ok((
        scheme,
        authority.host().to_ascii_lowercase(),
        authority.port_u16().unwrap_or(default_port),
    ))
}

/// Parse `allowed_origins` builder entries into their normalized form.
///
/// An entry that is not `"*"` or a bare `scheme://host[:port]` origin (see
/// [`parse_origin`] for exactly what that rejects) is logged at `warn` and
/// skipped. It could never match a real `Origin` header, so skipping it keeps
/// the check failing closed, and the warning names the entry so the
/// misconfiguration is visible at startup instead of as a silent lockout.
/// Allowlists usually come from runtime configuration, so a bad entry is not
/// treated as a programming error worth a panic.
pub(crate) fn parse_allowed_origins(entries: &[String]) -> Vec<AllowedOrigin> {
    entries
        .iter()
        .filter_map(|entry| {
            if entry == "*" {
                return Some(AllowedOrigin::Any);
            }
            match parse_origin(entry) {
                Ok((scheme, host, port)) => Some(AllowedOrigin::Origin { scheme, host, port }),
                Err(reason) => {
                    tracing::warn!(
                        entry = %entry,
                        "Ignoring invalid allowed_origins entry: {reason} (each entry must be \"*\" \
                         or a bare origin like \"https://example.com\" or \"https://example.com:8443\")"
                    );
                    None
                }
            }
        })
        .collect()
}

/// Validate the `Origin` header.
///
/// When `enabled`:
/// - Requests without an Origin header are allowed (same-origin, or a
///   non-browser client)
/// - Localhost origins are always allowed (DNS rebinding protection); a
///   configured `allowed_origins` list adds origins on top of this, it does
///   not replace it
/// - If `allowed_origins` is non-empty, non-localhost origins must match
///   (`"*"` matches any origin, including one that fails to parse)
/// - If `allowed_origins` is empty, non-localhost origins are rejected
///
/// Matching normalizes both sides the same way (see [`parse_origin`]), so
/// `https://example.com:443`, `https://Example.com`, and
/// `https://example.com/` in the allowlist all match a browser's
/// `https://example.com`. An incoming Origin that fails to parse (including
/// the opaque `null` origin) is rejected unless the allowlist contains
/// `"*"`.
///
/// Returns `Some(403 response)` if validation fails, `None` if it passes.
pub(crate) fn validate_origin(
    headers: &HeaderMap,
    enabled: bool,
    allowed_origins: &[AllowedOrigin],
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

        // "*" matches unconditionally, including an Origin that doesn't
        // even parse -- it means "no restriction", not "any origin that
        // happens to be well-formed".
        if allowed_origins
            .iter()
            .any(|o| matches!(o, AllowedOrigin::Any))
        {
            return None;
        }

        let matched = match parse_origin(origin_str) {
            Ok((scheme, host, port)) => allowed_origins.iter().any(|o| {
                matches!(
                    o,
                    AllowedOrigin::Origin {
                        scheme: s,
                        host: h,
                        port: p,
                    } if *s == scheme && *h == host && *p == port
                )
            }),
            // An Origin that doesn't parse (including the opaque `null`
            // origin) can't match a specific allowlist entry.
            Err(_) => false,
        };

        if !matched {
            tracing::warn!(origin = %origin_str, "Rejecting request: Origin not in allowlist");
            return Some((StatusCode::FORBIDDEN, "Origin not allowed").into_response());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_map(origin: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, origin.parse().unwrap());
        headers
    }

    /// Whether `origin` is accepted against an allowlist built from `entries`.
    fn is_allowed(origin: &str, entries: &[&str]) -> bool {
        let allowed =
            parse_allowed_origins(&entries.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        validate_origin(&header_map(origin), true, &allowed).is_none()
    }

    // Each row from the #1476 table: an allowlist entry that differs from
    // the browser's exact serialization must still match.
    #[test]
    fn default_https_port_normalizes() {
        assert!(is_allowed(
            "https://example.com",
            &["https://example.com:443"]
        ));
    }

    #[test]
    fn default_http_port_normalizes() {
        assert!(is_allowed("http://example.com", &["http://example.com:80"]));
    }

    #[test]
    fn host_case_normalizes() {
        assert!(is_allowed("https://example.com", &["https://Example.com"]));
    }

    #[test]
    fn trailing_slash_normalizes() {
        assert!(is_allowed("https://example.com", &["https://example.com/"]));
    }

    #[test]
    fn non_default_port_does_not_match() {
        assert!(!is_allowed(
            "https://example.com",
            &["https://example.com:8443"]
        ));
    }

    #[test]
    fn scheme_mismatch_does_not_match() {
        assert!(!is_allowed("https://example.com", &["http://example.com"]));
    }

    #[test]
    fn ws_and_wss_entries_normalize_to_the_browser_visible_scheme() {
        // Browsers never send Origin: ws://; the entry describes the
        // connection scheme of a WebSocket-only server, and is normalized
        // to the http(s) scheme a browser's Origin header actually carries.
        assert!(is_allowed("https://example.com", &["wss://example.com"]));
        assert!(is_allowed("http://example.com", &["ws://example.com"]));
        assert!(!is_allowed("http://example.com", &["wss://example.com"]));
    }

    #[test]
    fn wildcard_matches_anything_including_an_unparseable_origin() {
        assert!(is_allowed("https://example.com", &["*"]));
        assert!(is_allowed("not a valid origin $$", &["*"]));
    }

    #[test]
    fn unparseable_origin_is_rejected_without_a_wildcard() {
        assert!(!is_allowed(
            "not a valid origin $$",
            &["https://example.com"]
        ));
    }

    #[test]
    fn invalid_entries_are_rejected_with_a_reason() {
        for (entry, reason) in [
            ("https://example.com/app", "must not have a path"),
            ("https://example.com?x=1", "must not have a query"),
            ("https://example.com#frag", "has a fragment"),
            ("https://user:pass@example.com", "must not contain userinfo"),
            ("ftp://example.com", "unsupported scheme"),
            ("example.com", "has no scheme"),
        ] {
            let error = parse_origin(entry).expect_err(entry);
            assert!(error.contains(reason), "{entry}: {error}");
        }
    }

    #[test]
    fn invalid_entries_are_skipped_and_valid_ones_kept() {
        let parsed = parse_allowed_origins(&[
            "https://example.com/app".to_string(),
            "https://Example.com:443".to_string(),
            "ftp://example.com".to_string(),
            "*".to_string(),
        ]);
        assert_eq!(parsed.len(), 2, "{parsed:?}");
        assert!(matches!(parsed[1], AllowedOrigin::Any));
    }
}
