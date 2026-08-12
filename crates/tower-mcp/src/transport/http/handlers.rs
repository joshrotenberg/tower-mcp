//! The axum route handlers for [`HttpTransport`](super::HttpTransport), and
//! the request-validation / response-building helpers they share.
//!
//! `handle_post` is the main entry point and genuinely carries multiple
//! protocol lifecycles and dispatch paths in one function -- that reflects
//! the existing shape of the code (see #1242, which touched five separate
//! places in it), not something this split tries to untangle. `handle_get`,
//! `handle_delete`, and `handle_health` are the other route handlers, plus
//! both `subscriptions/listen` SSE paths available on this side of the
//! stateless/non-stateless line: the legacy session-based one
//! (`handle_subscriptions_listen_sse`) and the associated-request SSE
//! upgrade used for sampling (`associated_request_sse_response` /
//! `send_associated_request`). The modern (2026-07-28) `subscriptions/listen`
//! handler lives in `stateless_dispatch.rs` instead, since it only exists
//! when that feature is compiled in.
//!
//! Split out of `http.rs` in #1256 (phase 3).

use std::net::IpAddr;

use super::*;

type AssociatedCall = Pin<Box<dyn Future<Output = Result<JsonRpcResponse>> + Send + 'static>>;

pub(super) fn is_localhost_origin(origin: &str) -> bool {
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
pub(super) fn is_localhost_host(host: &str) -> bool {
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

/// Resolve the effective host for validation.
///
/// Prefers the `Host` header, falling back to the HTTP/2 `:authority`
/// pseudo-header (`request.uri().authority()`) when the header is missing.
/// This matters behind middleware like `axum::Router::nest`, which can
/// strip Hyper's synthesized `Host` before our handler sees it.
pub(super) fn effective_host<'a>(
    headers: &'a HeaderMap,
    uri: &'a axum::http::Uri,
) -> Option<&'a str> {
    if let Some(value) = headers.get(header::HOST)
        && let Ok(s) = value.to_str()
    {
        return Some(s);
    }
    uri.authority().map(|a| a.as_str())
}

/// Validate the `Host` header (defense-in-depth alongside Origin).
///
/// Returns Some(Response) if validation fails, None if it passes.
pub(super) fn validate_host(
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    state: &AppState,
) -> Option<Response> {
    if !state.validate_host {
        return None;
    }

    let Some(host) = effective_host(headers, uri) else {
        if state.allowed_hosts.is_empty() {
            // No Host header and no allowlist: fall back to permissive
            // behavior matching pre-validation defaults so we don't break
            // existing deployments. (Origin already protects browsers.)
            return None;
        }
        tracing::warn!("Rejecting request: missing Host header and no :authority fallback");
        return Some((StatusCode::BAD_REQUEST, "Missing Host header").into_response());
    };

    if is_localhost_host(host) {
        return None;
    }

    if state.allowed_hosts.is_empty() {
        // Non-localhost host with no explicit allowlist: keep accepting it.
        // Operators who want strict Host validation must opt in via
        // `.allowed_hosts(...)`. This preserves the historical behavior of
        // not enforcing Host on non-loopback deployments by default.
        return None;
    }

    if state.allowed_hosts.iter().any(|h| h == host) {
        return None;
    }

    tracing::warn!(host = %host, "Rejecting request: Host not in allowlist");
    Some((StatusCode::BAD_REQUEST, "Host not allowed").into_response())
}

/// Validate Origin header for security.
///
/// When origin validation is enabled:
/// - Requests without an Origin header are allowed (same-origin)
/// - Localhost origins are always allowed (DNS rebinding protection)
/// - If `allowed_origins` is non-empty, non-localhost origins must match
/// - If `allowed_origins` is empty, non-localhost origins are rejected
///
/// Returns Some(Response) if validation fails, None if it passes.
fn validate_origin(headers: &HeaderMap, state: &AppState) -> Option<Response> {
    if !state.validate_origin {
        return None;
    }

    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin_str = origin.to_str().unwrap_or("");

        // Always allow localhost origins (DNS rebinding protection allows these)
        if is_localhost_origin(origin_str) {
            return None;
        }

        // Non-localhost origin: check against allowed list
        if state.allowed_origins.is_empty() {
            tracing::warn!(
                origin = %origin_str,
                "Rejecting request: cross-origin not allowed (no allowlist configured)"
            );
            return Some(
                (StatusCode::FORBIDDEN, "Cross-origin requests not allowed").into_response(),
            );
        }

        if !state
            .allowed_origins
            .iter()
            .any(|o| o == origin_str || o == "*")
        {
            tracing::warn!(origin = %origin_str, "Rejecting request: Origin not in allowlist");
            return Some((StatusCode::FORBIDDEN, "Origin not allowed").into_response());
        }
    }

    None
}

/// Extract and validate session ID from headers
fn get_session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Extract protocol version from headers
fn get_protocol_version(headers: &HeaderMap) -> Option<String> {
    headers
        .get(MCP_PROTOCOL_VERSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Extract Last-Event-ID from headers for SSE stream resumption (SEP-1699)
fn get_last_event_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(LAST_EVENT_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
}

/// Check if the request is an initialize request
fn is_initialize_request(body: &serde_json::Value) -> bool {
    body.get("method")
        .and_then(|m| m.as_str())
        .map(|m| m == "initialize")
        .unwrap_or(false)
}

/// Check if this is a response to one of our outgoing requests
fn is_response(parsed: &serde_json::Value) -> bool {
    crate::framing::is_response_frame(parsed)
}

/// Resolve the selected tool's input schema when the transport owns an
/// [`McpRouter`]. Pre-built services do not expose their tool registry, so
/// supplied custom headers can still be checked there but missing headers
/// cannot be inferred before dispatch.
fn request_tool_input_schema(
    service_source: &ServiceSource,
    parsed: &serde_json::Value,
) -> Option<serde_json::Value> {
    if parsed.get("method").and_then(serde_json::Value::as_str) != Some("tools/call") {
        return None;
    }
    let name = parsed
        .get("params")
        .and_then(serde_json::Value::as_object)
        .and_then(|params| params.get("name"))
        .and_then(serde_json::Value::as_str)?;
    match service_source {
        ServiceSource::Router { router, .. } => router.tool_input_schema(name),
        ServiceSource::Service(_) => None,
    }
}

/// Return whether an HTTP request claims the modern, per-request-metadata
/// protocol era.
///
/// The body envelope is authoritative for era detection. The final-version
/// header is also treated as a modern claim so a missing or malformed
/// envelope receives the specified modern error instead of drifting into the
/// legacy session path.
fn claims_modern_protocol(headers: &HeaderMap, parsed: &serde_json::Value) -> bool {
    get_protocol_version(headers).as_deref() == Some(PROTOCOL_VERSION_2026_07_28)
        || parsed
            .get("params")
            .and_then(serde_json::Value::as_object)
            .and_then(|params| params.get("_meta"))
            .and_then(serde_json::Value::as_object)
            .is_some_and(|meta| meta.contains_key("io.modelcontextprotocol/protocolVersion"))
}

/// Validate the required modern per-request metadata and return its declared
/// protocol version.
///
/// `clientInfo` is deliberately optional in the final specification.
fn validate_modern_request_meta(
    parsed: &serde_json::Value,
) -> std::result::Result<String, JsonRpcError> {
    let params = parsed
        .get("params")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            JsonRpcError::invalid_params("Modern requests require a params object containing _meta")
        })?;
    let meta_value = params
        .get("_meta")
        .ok_or_else(|| JsonRpcError::invalid_params("Modern requests require a _meta object"))?;
    crate::protocol::validate_meta_object(meta_value)
        .map_err(|error| JsonRpcError::invalid_params(error.to_string()))?;
    let meta = meta_value
        .as_object()
        .expect("validate_meta_object accepted a JSON object");
    let protocol_version = meta
        .get("io.modelcontextprotocol/protocolVersion")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            JsonRpcError::invalid_params(
                "Missing or invalid _meta.io.modelcontextprotocol/protocolVersion",
            )
        })?;
    let client_capabilities = meta
        .get("io.modelcontextprotocol/clientCapabilities")
        .ok_or_else(|| {
            JsonRpcError::invalid_params("Missing _meta.io.modelcontextprotocol/clientCapabilities")
        })?;
    if !client_capabilities.is_object()
        || serde_json::from_value::<ClientCapabilities>(client_capabilities.clone()).is_err()
    {
        return Err(JsonRpcError::invalid_params(
            "Invalid _meta.io.modelcontextprotocol/clientCapabilities",
        ));
    }

    Ok(protocol_version.to_string())
}

/// Methods present in legacy protocol unions but removed from the modern core.
fn is_removed_modern_method(method: &str) -> bool {
    matches!(
        method,
        "initialize"
            | "notifications/initialized"
            | "ping"
            | "logging/setLevel"
            | "resources/subscribe"
            | "resources/unsubscribe"
            | "notifications/roots/list_changed"
    )
}

/// Extract request ID from a JSON value
pub(super) fn extract_request_id(parsed: &serde_json::Value) -> Option<RequestId> {
    parsed.get("id").and_then(|id| {
        if let Some(n) = id.as_i64() {
            Some(RequestId::Number(n))
        } else {
            id.as_str().map(|s| RequestId::String(s.to_string()))
        }
    })
}

/// Handle POST requests (JSON-RPC messages from client)
pub(super) async fn handle_post(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    let (parts, body_bytes) = request.into_parts();
    let headers = parts.headers;
    let uri = parts.uri.clone();

    // Validate Host (DNS rebinding defense, complement to Origin)
    if let Some(resp) = validate_host(&headers, &uri, &state) {
        return resp;
    }

    // Validate Origin
    if let Some(resp) = validate_origin(&headers, &state) {
        return resp;
    }

    // Bound the body size (rmcp #970 analog). axum's `DefaultBodyLimit`
    // doesn't apply here because this handler consumes the raw `Request`
    // instead of a body-consuming extractor, so this is the only limit on
    // the MCP POST body. A declared Content-Length above the limit is
    // rejected without reading; chunked bodies are capped while streaming.
    if let Some(declared) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        && declared > state.max_body_size
    {
        return body_too_large_response(state.max_body_size);
    }

    let body = match axum::body::to_bytes(body_bytes, state.max_body_size).await {
        Ok(bytes) => match String::from_utf8(bytes.to_vec()) {
            Ok(s) => s,
            Err(e) => {
                return json_rpc_error_response(
                    None,
                    JsonRpcError::parse_error(format!("Invalid UTF-8: {}", e)),
                );
            }
        },
        Err(e) if is_length_limit_error(&e) => {
            return body_too_large_response(state.max_body_size);
        }
        Err(e) => {
            return json_rpc_error_response(
                None,
                JsonRpcError::parse_error(format!("Failed to read body: {}", e)),
            );
        }
    };

    // Per-request data bridged from HTTP into MCP extensions: OAuth claims
    // when that feature is compiled in, plus whatever types the server
    // registered with `bridge_extension`, which is independent of OAuth
    // (#1242). Always bound, since the bridges run in every build.
    let http_extensions = parts.extensions;

    // Parse the request body
    let parsed: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return json_rpc_error_response(
                None,
                JsonRpcError::parse_error(format!("Invalid JSON: {}", e)),
            );
        }
    };

    // A version header supplies enough exact context to reject a batch before
    // any object-only HTTP classification runs. Legacy batches without a
    // header are validated against their session revision after lookup below.
    if parsed.is_array()
        && let Some(version) = get_protocol_version(&headers)
    {
        let revision = match version.parse::<McpProtocolRevision>() {
            Ok(revision) => revision,
            Err(_) => {
                return json_rpc_error_response(
                    None,
                    JsonRpcError::unsupported_protocol_version(
                        version,
                        state.protocol_support.versions().iter().map(String::as_str),
                    ),
                );
            }
        };
        if let Err(error) = inspect_runtime_value(
            &parsed,
            revision,
            &state.protocol_support,
            McpDirection::ClientToServer,
        ) {
            let status = if revision == McpProtocolRevision::V2026_07_28 {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::OK
            };
            return json_rpc_error_response_with_status(None, error, status);
        }
    }

    // Check if this is an initialize request (creates new session)
    let is_init = is_initialize_request(&parsed);
    let request_method = parsed
        .get("method")
        .and_then(|method| method.as_str())
        .unwrap_or_default()
        .to_string();
    let tool_input_schema = request_tool_input_schema(&state.service_source, &parsed);
    let modern_request = claims_modern_protocol(&headers, &parsed);

    // The modern protocol is selected by its per-request `_meta` envelope,
    // with the final-version HTTP header also acting as a signal for malformed
    // requests whose envelope is missing. Resolve that era before consulting
    // any legacy session state so modern traffic cannot accidentally fall
    // through to the initialize/session lifecycle.
    if modern_request {
        let id = extract_request_id(&parsed);
        let body_version = match validate_modern_request_meta(&parsed) {
            Ok(version) => version,
            Err(error) => {
                return json_rpc_error_response_with_status(id, error, StatusCode::BAD_REQUEST);
            }
        };

        let Some(header_version) = get_protocol_version(&headers) else {
            return json_rpc_error_response_with_status(
                id,
                JsonRpcError::header_mismatch("MCP-Protocol-Version header is required"),
                StatusCode::BAD_REQUEST,
            );
        };
        if header_version != body_version {
            return json_rpc_error_response_with_status(
                id,
                JsonRpcError::header_mismatch(format!(
                    "MCP-Protocol-Version header value {header_version:?} does not match \
                     request _meta protocol version {body_version:?}"
                )),
                StatusCode::BAD_REQUEST,
            );
        }

        if !state.protocol_support.contains(&body_version) {
            return json_rpc_error_response_with_status(
                id,
                JsonRpcError::unsupported_protocol_version(
                    body_version,
                    state.protocol_support.versions().iter().map(String::as_str),
                ),
                StatusCode::BAD_REQUEST,
            );
        }

        let revision = match body_version.parse::<McpProtocolRevision>() {
            Ok(revision) => revision,
            Err(_) => {
                return json_rpc_error_response_with_status(
                    id,
                    JsonRpcError::unsupported_protocol_version(
                        body_version,
                        state.protocol_support.versions().iter().map(String::as_str),
                    ),
                    StatusCode::BAD_REQUEST,
                );
            }
        };
        if let Err(error) = inspect_runtime_value(
            &parsed,
            revision,
            &state.protocol_support,
            McpDirection::ClientToServer,
        ) {
            return json_rpc_error_response_with_status(id, error, StatusCode::BAD_REQUEST);
        }

        let sep_2243_mode = crate::transport::http_headers::mode_for_version(&body_version);
        if let Err(error) = crate::transport::http_headers::validate_with_tool_schema(
            &headers,
            &parsed,
            sep_2243_mode,
            tool_input_schema.as_ref(),
        ) {
            tracing::warn!(
                mode = ?sep_2243_mode,
                version = %body_version,
                error = %error.message,
                "Rejecting modern request: HTTP header validation failed",
            );
            return json_rpc_error_response_with_status(id, error, StatusCode::BAD_REQUEST);
        }

        if is_removed_modern_method(&request_method) {
            return json_rpc_error_response_with_status(
                id,
                JsonRpcError::method_not_found(&request_method),
                StatusCode::NOT_FOUND,
            );
        }
    }

    // SEP-2575 / SEP-2567: version-gated stateless mode for 2026-07-28+ clients.
    //
    // When the requested (or carried) protocol version is >= 2026-07-28 and the
    // request has no mcp-session-id, every request -- including `initialize` --
    // is served without creating or looking up a session. Each request is fully
    // self-contained; client identity and capabilities flow through per-request
    // `_meta` rather than a session handshake.
    //
    // This block runs before the legacy SEP-1442 stateless path so that
    // 2026-07-28 requests are handled here regardless of whether
    // `stateless_config` is set on the transport.
    #[cfg(feature = "stateless")]
    {
        let version_in_play: Option<String> = if is_init && !modern_request {
            // For `initialize`, read the version the client is requesting from
            // the params object.
            parsed
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        } else {
            // For non-init requests, only the HTTP-level `MCP-Protocol-Version`
            // header gates stateless mode. Body-level `_meta.protocolVersion` is
            // plumbed to handlers via `stash_per_request_meta` in both paths.
            get_protocol_version(&headers)
        };

        if let Some(ref version) = version_in_play
            && is_stateless_protocol_version(version)
            && state.protocol_support.contains(version)
            // `subscriptions/listen` opens an SSE stream; let it fall through to the
            // dedicated intercept below rather than handling it as a plain RPC call.
            && parsed.get("method").and_then(|m| m.as_str()) != Some("subscriptions/listen")
        {
            // Notifications and responses are fire-and-forget; no dispatch needed.
            if !is_init && (parsed.get("id").is_none() || is_response(&parsed)) {
                return StatusCode::ACCEPTED.into_response();
            }

            // SEP-2243 validation before `parsed` is consumed by deserialization.
            // 2026-07-28 falls into strict mode, so missing Mcp-Method is an error.
            let sep_2243_mode = crate::transport::http_headers::mode_for_version(version);
            if let Err(err) = crate::transport::http_headers::validate_with_tool_schema(
                &headers,
                &parsed,
                sep_2243_mode,
                tool_input_schema.as_ref(),
            ) {
                tracing::warn!(
                    mode = ?sep_2243_mode,
                    version = %version,
                    error = %err.message,
                    "Rejecting stateless request: SEP-2243 header validation failed",
                );
                let id = extract_request_id(&parsed);
                let mut resp = json_rpc_error_response(id, err);
                *resp.status_mut() = StatusCode::BAD_REQUEST;
                return resp;
            }

            let request: JsonRpcRequest = match serde_json::from_value(parsed) {
                Ok(r) => r,
                Err(e) => {
                    return json_rpc_error_response(
                        None,
                        JsonRpcError::parse_error(format!("Invalid request: {}", e)),
                    );
                }
            };

            // Ephemeral pre-initialized service -- no session is stored or created.
            //
            // A per-request notification channel captures anything the handler
            // emits during the call (progress, logging). With no session and no
            // GET stream on this path, those messages can only reach the client
            // on the POST response itself: per the draft Streamable HTTP rules,
            // a plain JSON body is only correct when the first outbound message
            // is the terminal response; otherwise the response falls back to
            // SSE with the notifications delivered ahead of the terminal
            // response.
            // Captured before the match below borrows `router` into the
            // ephemeral session; used to stamp `_meta.serverInfo` on the
            // outgoing response (SEP-2575). `None` for a transport built
            // from a pre-built service (no router to read identity from).
            let server_identity = match &state.service_source {
                ServiceSource::Router { router, .. } if state.stamp_server_info => {
                    Some(router.implementation())
                }
                _ => None,
            };

            let (notif_tx, mut notif_rx) = crate::context::notification_channel(64);
            let mut service = match &state.service_source {
                ServiceSource::Router { router, factory } => {
                    let ephemeral = router
                        .with_fresh_session()
                        .with_request_notification_sender(notif_tx);
                    ephemeral.session().mark_preinitialized();
                    JsonRpcService::new(factory(ephemeral))
                }
                ServiceSource::Service(mutex) => JsonRpcService::new(mutex.lock().unwrap().clone()),
            };

            let mut ext = crate::router::Extensions::new();
            ext.insert(state.protocol_support.clone());
            #[cfg(feature = "oauth")]
            if let Some(claims) = http_extensions.get::<crate::oauth::token::TokenClaims>() {
                ext.insert(claims.clone());
            }
            stash_per_request_meta(&request, &mut ext);
            crate::transport::extension_bridge::apply_extension_bridges(
                &state.extension_bridges,
                &http_extensions,
                &mut ext,
            );

            // rmcp #967 analog: give the request a cancellation token that
            // fires if the client disconnects before the response is
            // delivered. The router adopts the token as the
            // `RequestContext`'s cancellation source, so handlers observe
            // the disconnect via `ctx.is_cancelled()` / `ctx.cancelled()`,
            // and spawned work holding a token clone is signalled even
            // after the request future itself is dropped. Session-based
            // requests are exempt: with stream resumption, a disconnect is
            // not a cancellation.
            let cancel_token = crate::context::CancellationToken::new();
            let mut cancel_guard = CancelOnDisconnect::arm(cancel_token.clone());
            ext.insert(cancel_token);

            service = service.with_extensions(ext);

            let mut call: std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::error::Result<JsonRpcResponse>> + Send>,
            > = Box::pin(async move {
                let mut service = service;
                service.call_single(request).await
            });

            enum FirstOutbound {
                Response(crate::error::Result<JsonRpcResponse>),
                Notification(crate::context::ServerNotification),
            }

            // Race the handler against its first notification. A closed
            // channel (no sender attached, or all senders dropped) simply
            // awaits the handler.
            let first = loop {
                let outbound = tokio::select! {
                    // A handler may enqueue a notification and complete in
                    // the same poll. Observe the queued notification first so
                    // it is neither dropped nor raced behind the response.
                    biased;
                    maybe = notif_rx.recv() => match maybe {
                        Some(n) => FirstOutbound::Notification(n),
                        None => FirstOutbound::Response((&mut call).await),
                    },
                    result = &mut call => FirstOutbound::Response(result),
                };
                match outbound {
                    FirstOutbound::Notification(notification)
                        if state.modern_subscriptions.publish(&notification) =>
                    {
                        continue;
                    }
                    outbound => break outbound,
                }
            };

            match first {
                FirstOutbound::Response(result) => {
                    // `select!` may observe a handler's ready response in the
                    // same poll that the handler enqueued notifications.
                    // Drain that queue before committing a JSON response.
                    while let Ok(notification) = notif_rx.try_recv() {
                        if state.modern_subscriptions.publish(&notification) {
                            continue;
                        }
                        let ready_call: std::pin::Pin<
                            Box<
                                dyn std::future::Future<
                                        Output = crate::error::Result<JsonRpcResponse>,
                                    > + Send,
                            >,
                        > = Box::pin(async move { result });
                        let mut resp = stateless_sse_with_notifications(
                            notification,
                            ready_call,
                            notif_rx,
                            StatelessSseContext {
                                version: version.clone(),
                                method: request_method.clone(),
                                cancel_guard,
                                server_identity,
                                subscriptions: state.modern_subscriptions.clone(),
                            },
                        );
                        resp.headers_mut().insert(
                            MCP_PROTOCOL_VERSION_HEADER,
                            HeaderValue::from_str(version).unwrap(),
                        );
                        return resp;
                    }

                    // Handler finished; the response is about to be
                    // produced, so dropping the connection from here on is
                    // no longer a cancellation.
                    cancel_guard.disarm();
                    let mut response = match result {
                        Ok(resp) => resp,
                        Err(e) => {
                            return json_rpc_error_response(
                                None,
                                JsonRpcError::internal_error(e.to_string()),
                            );
                        }
                    };

                    // Keep the response aligned with the version selected for
                    // this sessionless request. The router also receives the
                    // runtime allow-list through Extensions.
                    if is_init
                        && let JsonRpcResponse::Result(ref mut result) = response
                        && let Some(pv) = result.result.get_mut("protocolVersion")
                    {
                        *pv = serde_json::Value::String(version.clone());
                    }
                    apply_protocol_result_fields(&mut response, &request_method, version);
                    if let Some(ref identity) = server_identity {
                        stamp_server_info(&mut response, identity);
                    }

                    let status = modern_response_status(&response);
                    let mut resp = if state.sse_responses {
                        sse_json_response(&response)
                    } else {
                        axum::Json(response).into_response()
                    };
                    *resp.status_mut() = status;
                    resp.headers_mut().insert(
                        MCP_PROTOCOL_VERSION_HEADER,
                        HeaderValue::from_str(version).unwrap(),
                    );
                    // Intentionally NO `mcp-session-id` header for 2026-07-28+ clients.
                    return resp;
                }
                FirstOutbound::Notification(first_notif) => {
                    let mut resp = stateless_sse_with_notifications(
                        first_notif,
                        call,
                        notif_rx,
                        StatelessSseContext {
                            version: version.clone(),
                            method: request_method.clone(),
                            cancel_guard,
                            server_identity,
                            subscriptions: state.modern_subscriptions.clone(),
                        },
                    );
                    resp.headers_mut().insert(
                        MCP_PROTOCOL_VERSION_HEADER,
                        HeaderValue::from_str(version).unwrap(),
                    );
                    // Intentionally NO `mcp-session-id` header for 2026-07-28+ clients.
                    return resp;
                }
            }
        }
    }

    // SEP-1442: Handle stateless requests (no session needed).
    // Stateless requests have a protocol version but no session ID and are not
    // initialize requests. They are processed with an ephemeral service and
    // return immediately without storing any session state.
    #[cfg(feature = "stateless")]
    if !is_init && state.stateless_config.is_some() && get_session_id(&headers).is_none() {
        let version_from_header = get_protocol_version(&headers);
        let params = parsed.get("params").unwrap_or(&parsed);
        let version_from_meta = crate::stateless::StatelessRequestMeta::from_params(params)
            .and_then(|m| m.protocol_version);

        if let Some(version) = version_from_header.or(version_from_meta) {
            if let Err(err) = crate::stateless::validate_protocol_version(&version) {
                return json_rpc_error_response(None, err);
            }

            // Notifications and responses don't make sense without a session
            if parsed.get("id").is_none() || is_response(&parsed) {
                return StatusCode::ACCEPTED.into_response();
            }

            let request: JsonRpcRequest = match serde_json::from_value(parsed) {
                Ok(r) => r,
                Err(e) => {
                    return json_rpc_error_response(
                        None,
                        JsonRpcError::parse_error(format!("Invalid request: {}", e)),
                    );
                }
            };

            // Ephemeral pre-initialized service -- no session stored
            let mut service = match &state.service_source {
                ServiceSource::Router { router, factory } => {
                    let ephemeral = router.with_fresh_session();
                    ephemeral.session().mark_preinitialized();
                    JsonRpcService::new(factory(ephemeral))
                }
                ServiceSource::Service(mutex) => JsonRpcService::new(mutex.lock().unwrap().clone()),
            };

            let mut ext = crate::router::Extensions::new();
            ext.insert(state.protocol_support.clone());
            #[cfg(feature = "oauth")]
            if let Some(claims) = http_extensions.get::<crate::oauth::token::TokenClaims>() {
                ext.insert(claims.clone());
            }
            #[cfg(feature = "stateless")]
            stash_per_request_meta(&request, &mut ext);
            crate::transport::extension_bridge::apply_extension_bridges(
                &state.extension_bridges,
                &http_extensions,
                &mut ext,
            );
            if !ext.is_empty() {
                service = service.with_extensions(ext);
            }

            let mut response = match service.call_single(request).await {
                Ok(resp) => resp,
                Err(e) => {
                    return json_rpc_error_response(
                        None,
                        JsonRpcError::internal_error(e.to_string()),
                    );
                }
            };
            apply_protocol_result_fields(&mut response, &request_method, &version);

            let mut resp = if state.sse_responses {
                sse_json_response(&response)
            } else {
                axum::Json(response).into_response()
            };
            resp.headers_mut().insert(
                MCP_PROTOCOL_VERSION_HEADER,
                HeaderValue::from_str(&version).unwrap(),
            );
            return resp;
        }
    }

    // Final-protocol subscriptions are sessionless long-lived POSTs. They
    // must be established before consulting any legacy session state.
    #[cfg(feature = "stateless")]
    if modern_request && request_method == "subscriptions/listen" {
        return handle_modern_subscriptions_listen_sse(state, &parsed, &http_extensions).await;
    }

    // Runtime allowlist enforcement precedes semantic profile validation.
    // This is especially important for optional-session traffic: an unknown
    // header must not be interpreted under a fallback revision.
    if !is_init
        && let Some(version) = get_protocol_version(&headers)
        && !state.protocol_support.contains(&version)
    {
        return json_rpc_error_response(
            extract_request_id(&parsed),
            JsonRpcError::unsupported_protocol_version(
                version,
                state.protocol_support.versions().iter().map(String::as_str),
            ),
        );
    }

    let uses_transient_session = !is_init
        && !modern_request
        && get_session_id(&headers).is_none()
        && state.optional_sessions;

    // Get or create session
    let session = if is_init {
        // Create new session for initialize
        let create_result = match &state.service_source {
            ServiceSource::Router { router, factory } => {
                // Use with_fresh_session() to ensure each session has its own state
                state
                    .sessions
                    .create(router.with_fresh_session(), factory.clone())
                    .await
            }
            ServiceSource::Service(mutex) => {
                let service = mutex.lock().unwrap().clone();
                state.sessions.create_from_service(service).await
            }
        };
        match create_result {
            Some(s) => s,
            None => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Maximum session limit reached",
                )
                    .into_response();
            }
        }
    } else if !modern_request && let Some(session_id) = get_session_id(&headers) {
        // Client sent a session ID -- look it up
        match state.sessions.get(&session_id).await {
            Some(s) => s,
            None => {
                // Return JSON-RPC error with session info so clients know to re-initialize
                return json_rpc_error_response(
                    None,
                    JsonRpcError::session_not_found_with_id(&session_id),
                );
            }
        }
    } else if state.optional_sessions {
        // No session ID, but sessions are optional -- create a transient,
        // pre-initialized session so the router won't reject the request.
        // This supports clients (Codex CLI, Cursor, etc.) that perform
        // initialize + tools/list during setup but don't carry the session
        // ID forward to subsequent requests.
        let create_result = match &state.service_source {
            ServiceSource::Router { router, factory } => {
                state
                    .sessions
                    .create_initialized(router.with_fresh_session(), factory.clone())
                    .await
            }
            ServiceSource::Service(mutex) => {
                let service = mutex.lock().unwrap().clone();
                state
                    .sessions
                    .create_initialized_from_service(service)
                    .await
            }
        };
        match create_result {
            Some(s) => s,
            None => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Maximum session limit reached",
                )
                    .into_response();
            }
        }
    } else {
        // No session ID and sessions are required
        return json_rpc_error_response(None, JsonRpcError::session_required());
    };

    // Session lookup establishes the exact legacy revision. Validate the raw
    // envelope before object-only notification/response routing, then let the
    // existing request dispatcher consume the typed shape.
    let session_protocol_version = if uses_transient_session {
        let version = state
            .protocol_support
            .versions()
            .iter()
            .find(|version| {
                crate::protocol::SUPPORTED_PROTOCOL_VERSIONS.contains(&version.as_str())
            })
            .map_or_else(
                || state.protocol_support.preferred().to_string(),
                Clone::clone,
            );
        *session.protocol_version.write().await = version.clone();
        version
    } else {
        session.protocol_version.read().await.clone()
    };
    let session_revision = match session_protocol_version.parse::<McpProtocolRevision>() {
        Ok(revision) => revision,
        Err(_) => {
            return json_rpc_error_response(
                extract_request_id(&parsed),
                JsonRpcError::unsupported_protocol_version(
                    session_protocol_version,
                    state.protocol_support.versions().iter().map(String::as_str),
                ),
            );
        }
    };
    if !is_init
        && let Err(error) = inspect_runtime_value(
            &parsed,
            session_revision,
            &state.protocol_support,
            McpDirection::ClientToServer,
        )
    {
        return json_rpc_error_response(extract_request_id(&parsed), error);
    }

    if parsed.is_array() {
        if state.strict_initialization
            && !session
                .initialized_notification_received
                .load(Ordering::Acquire)
        {
            return json_rpc_error_response(
                None,
                JsonRpcError::invalid_request(
                    "Client must send notifications/initialized before making requests",
                ),
            );
        }

        let message: JsonRpcMessage = match serde_json::from_value(parsed) {
            Ok(message) => message,
            Err(error) => {
                return json_rpc_error_response(
                    None,
                    JsonRpcError::invalid_request(format!("Invalid request batch: {error}")),
                );
            }
        };

        let mut extensions = crate::router::Extensions::new();
        extensions.insert(state.protocol_support.clone());
        extensions.insert(session_revision);
        #[cfg(feature = "oauth")]
        if let Some(claims) = http_extensions.get::<crate::oauth::token::TokenClaims>() {
            extensions.insert(claims.clone());
        }
        crate::transport::extension_bridge::apply_extension_bridges(
            &state.extension_bridges,
            &http_extensions,
            &mut extensions,
        );

        let mut service = JsonRpcService::new(session.make_service())
            .with_extensions(extensions)
            .protocol_support(state.protocol_support.clone())
            .with_negotiated_protocol_version(&session_protocol_version);
        let response = match service.call_message(message).await {
            Ok(response) => response,
            Err(error) => {
                return json_rpc_error_response(
                    None,
                    JsonRpcError::internal_error(error.to_string()),
                );
            }
        };
        let mut response = axum::Json(response).into_response();
        response.headers_mut().insert(
            MCP_PROTOCOL_VERSION_HEADER,
            HeaderValue::from_str(&session_protocol_version).unwrap(),
        );
        return response;
    }

    // SEP-2575 / SEP-2567: intercept `subscriptions/listen` before the standard
    // version validation. `subscriptions/listen` is only available when the
    // effective protocol version is >= 2026-07-28; otherwise we return a
    // proper JSON-RPC error rather than silently falling through to the
    // router (which would return `MethodNotFound` anyway, but without the
    // protocol-version context).
    //
    // We check the Mcp-Protocol-Version header first (per-request override),
    // falling back to the session-negotiated version. Intercepting here
    // also prevents the version-validation guard below from rejecting the
    // 2026-07-28 header before we can inspect it.
    {
        let method_str = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");
        if method_str == "subscriptions/listen" {
            let req_id = extract_request_id(&parsed);
            let effective_version = if let Some(v) = get_protocol_version(&headers) {
                v
            } else {
                session.protocol_version.read().await.clone()
            };
            if version_supports_subscriptions_listen(&effective_version, &state.protocol_support) {
                return handle_subscriptions_listen_sse(session).await;
            } else {
                return json_rpc_error_response(
                    req_id,
                    JsonRpcError::method_not_found("subscriptions/listen"),
                );
            }
        }
    }

    // SEP-2243: validate the standardized HTTP headers (Mcp-Method,
    // Mcp-Name, Mcp-Param-*) against the body. Mode is "strict" only
    // when the negotiated protocol version is at or beyond the
    // SEP-2243-inclusion version; otherwise present headers are still
    // checked for body consistency but missing headers are allowed.
    //
    // For `initialize` requests the session's protocol version hasn't
    // been negotiated yet, so we fall back to the version the client
    // requested in the body. For all other requests we use the session's
    // negotiated version (which is also reflected back in the response
    // `Mcp-Protocol-Version` header).
    let sep_2243_version = if is_init {
        match parsed
            .get("params")
            .and_then(|p| p.get("protocolVersion"))
            .and_then(|v| v.as_str())
        {
            Some(v) => v.to_string(),
            None => session.protocol_version.read().await.clone(),
        }
    } else {
        session.protocol_version.read().await.clone()
    };
    let sep_2243_mode = crate::transport::http_headers::mode_for_version(&sep_2243_version);
    if let Err(err) = crate::transport::http_headers::validate_with_tool_schema(
        &headers,
        &parsed,
        sep_2243_mode,
        tool_input_schema.as_ref(),
    ) {
        tracing::warn!(
            mode = ?sep_2243_mode,
            version = %sep_2243_version,
            error = %err.message,
            "Rejecting request: SEP-2243 header validation failed",
        );
        let id = extract_request_id(&parsed);
        let mut resp = json_rpc_error_response(id, err);
        // Per SEP-2243 §"Error Code" the HTTP status MUST be 400.
        *resp.status_mut() = StatusCode::BAD_REQUEST;
        return resp;
    }

    // Check if this is a response to one of our outgoing requests (sampling)
    if is_response(&parsed) {
        if let Some(id) = extract_request_id(&parsed) {
            let result = if let Some(error) = parsed.get("error") {
                let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
                let message = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error");
                Err(Error::Internal(format!(
                    "Client error ({}): {}",
                    code, message
                )))
            } else if let Some(result) = parsed.get("result") {
                Ok(result.clone())
            } else {
                Err(Error::Internal(
                    "Response has neither result nor error".to_string(),
                ))
            };

            if session.complete_pending_request(&id, result).await {
                tracing::debug!(request_id = ?id, "Completed pending request");
            } else {
                tracing::warn!(request_id = ?id, "Received response for unknown request");
            }
        }
        return StatusCode::ACCEPTED.into_response();
    }

    // Check if this is a notification (no id field)
    if parsed.get("id").is_none() {
        // Handle notification
        if let Ok(notification) = serde_json::from_value::<JsonRpcNotification>(parsed)
            && let Ok(mcp_notification) = McpNotification::from_jsonrpc(&notification)
        {
            // Per the MCP 2025-11-25 spec, clients MUST send
            // `notifications/initialized` after receiving the `initialize`
            // response and before sending any other requests. Record the
            // receipt so the strict_initialization check below can allow
            // subsequent tool/resource/prompt requests.
            if matches!(&mcp_notification, McpNotification::Initialized) {
                session
                    .initialized_notification_received
                    .store(true, Ordering::Release);
                tracing::debug!(session_id = %session.id, "Received notifications/initialized");
            }
            session.handle_notification(mcp_notification);
        }
        return StatusCode::ACCEPTED.into_response();
    }

    // Enforce `notifications/initialized` before any non-initialize request
    // (MCP 2025-11-25 spec requirement). This only applies to the session-based
    // path; stateless requests (2026-07-28) are handled above and never reach here.
    if !is_init
        && state.strict_initialization
        && !session
            .initialized_notification_received
            .load(Ordering::Acquire)
    {
        let id = extract_request_id(&parsed);
        tracing::warn!(
            session_id = %session.id,
            "Rejecting request: notifications/initialized not yet received"
        );
        return json_rpc_error_response(
            id,
            JsonRpcError::invalid_request(
                "Client must send notifications/initialized before making requests",
            ),
        );
    }

    // For initialize requests, capture the advertised client info /
    // capabilities from the raw params before `parsed` is consumed by
    // deserialization. These are stashed onto the live `Session` after a
    // successful initialize so the persisted SessionRecord faithfully
    // describes the client (rather than carrying the defaults set at
    // session-create time).
    let init_client_metadata: Option<(Option<Implementation>, Option<ClientCapabilities>)> =
        if is_init {
            let params = parsed.get("params");
            let client_info = params
                .and_then(|p| p.get("clientInfo"))
                .and_then(|v| serde_json::from_value::<Implementation>(v.clone()).ok());
            let client_capabilities = params
                .and_then(|p| p.get("capabilities"))
                .and_then(|v| serde_json::from_value::<ClientCapabilities>(v.clone()).ok());
            Some((client_info, client_capabilities))
        } else {
            None
        };

    // Handle as JSON-RPC request
    let request: JsonRpcRequest = match serde_json::from_value(parsed) {
        Ok(r) => r,
        Err(e) => {
            return json_rpc_error_response(
                None,
                JsonRpcError::parse_error(format!("Invalid request: {}", e)),
            );
        }
    };

    // Process the request through the middleware-wrapped service
    let mut service = JsonRpcService::new(session.make_service());

    // Bridge per-request data from HTTP into MCP Extensions: OAuth claims,
    // SEP-2575 `_meta` (clientInfo, clientCapabilities, etc.). Empty ext is
    // skipped to avoid pointless allocation.
    #[allow(unused_mut)]
    let mut ext = crate::router::Extensions::new();
    ext.insert(state.protocol_support.clone());
    ext.insert(session_revision);
    #[cfg(feature = "oauth")]
    if let Some(claims) = http_extensions.get::<crate::oauth::token::TokenClaims>() {
        ext.insert(claims.clone());
    }
    crate::transport::extension_bridge::apply_extension_bridges(
        &state.extension_bridges,
        &http_extensions,
        &mut ext,
    );
    #[cfg(feature = "stateless")]
    stash_per_request_meta(&request, &mut ext);

    // SEP-2260: legacy server-to-client requests are associated with the
    // client POST that caused them. Give this request its own channel while
    // drawing IDs from the session-wide allocator so concurrent POSTs cannot
    // collide or leak requests onto one another's response streams.
    let mut associated_request_rx = if !is_init {
        session.request_id_allocator.as_ref().map(|next_id| {
            let (request_tx, request_rx) = outgoing_request_channel(32);
            let requester: ClientRequesterHandle = Arc::new(
                ChannelClientRequester::with_id_allocator(request_tx, next_id.clone()),
            );
            ext.insert(requester);
            request_rx
        })
    } else {
        None
    };

    if !ext.is_empty() {
        service = service.with_extensions(ext);
    }

    let request_id = request.id.clone();
    let mut call: AssociatedCall = Box::pin(async move { service.call_single(request).await });
    let mut response = if let Some(mut request_rx) = associated_request_rx.take() {
        tokio::select! {
            result = &mut call => match result {
                Ok(response) => response,
                Err(error) => {
                    return json_rpc_error_response(
                        Some(request_id),
                        JsonRpcError::internal_error(error.to_string()),
                    );
                }
            },
            outgoing = request_rx.recv() => {
                match outgoing {
                    Some(outgoing) => {
                        let negotiated_version = session.protocol_version.read().await.clone();
                        return associated_request_sse_response(
                            session,
                            call,
                            request_rx,
                            outgoing,
                            request_id,
                            request_method,
                            negotiated_version,
                        );
                    }
                    None => match call.await {
                        Ok(response) => response,
                        Err(error) => {
                            return json_rpc_error_response(
                                Some(request_id),
                                JsonRpcError::internal_error(error.to_string()),
                            );
                        }
                    },
                }
            }
        }
    } else {
        match call.await {
            Ok(response) => response,
            Err(error) => {
                return json_rpc_error_response(
                    Some(request_id),
                    JsonRpcError::internal_error(error.to_string()),
                );
            }
        }
    };

    // For successful initialize responses, extract and store the negotiated
    // protocol version, stash the client's advertised identity / capabilities
    // on the live session, and persist the now-complete record to the session
    // store so a restore from a peer instance sees the original client info
    // instead of defaults.
    if is_init && let JsonRpcResponse::Result(ref result) = response {
        if let Some(version) = result
            .result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
        {
            *session.protocol_version.write().await = version.to_string();
        }
        if let Some((client_info, client_capabilities)) = init_client_metadata {
            *session.client_info.write().await = client_info;
            *session.client_capabilities.write().await = client_capabilities;
        }
        state.sessions.save_record(&session).await;
    }

    let negotiated_version = session.protocol_version.read().await.clone();
    let response_version = if request_method == "server/discover"
        && state.protocol_support.contains(PROTOCOL_VERSION_2026_07_28)
    {
        PROTOCOL_VERSION_2026_07_28
    } else {
        &negotiated_version
    };
    apply_protocol_result_fields(&mut response, &request_method, response_version);

    // Build response with headers
    let mut resp = if state.sse_responses {
        sse_json_response(&response)
    } else {
        axum::Json(response).into_response()
    };

    if is_init {
        resp.headers_mut().insert(
            MCP_SESSION_ID_HEADER,
            HeaderValue::from_str(&session.id).unwrap(),
        );
    }

    // Always include the negotiated protocol version header
    resp.headers_mut().insert(
        MCP_PROTOCOL_VERSION_HEADER,
        HeaderValue::from_str(&negotiated_version).unwrap(),
    );

    resp
}

/// Keep legacy server-to-client requests on the POST response stream that
/// caused them. These events deliberately have no SSE IDs and are not written
/// to the session event store: their response channels only exist on this
/// process and replaying them on another connection would break association.
fn associated_request_sse_response(
    session: Arc<Session>,
    mut call: AssociatedCall,
    mut request_rx: OutgoingRequestReceiver,
    first_outgoing: OutgoingRequest,
    original_request_id: RequestId,
    request_method: String,
    negotiated_version: String,
) -> Response {
    let (event_tx, event_rx) =
        tokio::sync::mpsc::channel::<std::result::Result<Event, Infallible>>(32);
    let call_version = negotiated_version.clone();

    tokio::spawn(async move {
        let mut pending_ids = Vec::new();
        if !send_associated_request(&session, &event_tx, first_outgoing, &mut pending_ids).await {
            session
                .fail_pending_requests(
                    &pending_ids,
                    "originating POST disconnected before the client request was delivered",
                )
                .await;
            return;
        }

        let mut requests_open = true;
        loop {
            tokio::select! {
                _ = event_tx.closed() => {
                    session
                        .fail_pending_requests(
                            &pending_ids,
                            "originating POST response stream disconnected",
                        )
                        .await;
                    return;
                }
                result = &mut call => {
                    session
                        .fail_pending_requests(
                            &pending_ids,
                            "originating POST completed before the client request response arrived",
                        )
                        .await;

                    let mut response = match result {
                        Ok(response) => response,
                        Err(error) => JsonRpcResponse::error(
                            Some(original_request_id),
                            JsonRpcError::internal_error(error.to_string()),
                        ),
                    };
                    apply_protocol_result_fields(
                        &mut response,
                        &request_method,
                        &call_version,
                    );

                    match serde_json::to_string(&response) {
                        Ok(data) => {
                            let _ = event_tx
                                .send(Ok(
                                    Event::default()
                                        .event(SSE_MESSAGE_EVENT)
                                        .data(data),
                                ))
                                .await;
                        }
                        Err(error) => {
                            tracing::error!(
                                error = %error,
                                "Failed to serialize associated POST response",
                            );
                        }
                    }
                    return;
                }
                outgoing = request_rx.recv(), if requests_open => {
                    match outgoing {
                        Some(outgoing) => {
                            if !send_associated_request(
                                &session,
                                &event_tx,
                                outgoing,
                                &mut pending_ids,
                            )
                            .await
                            {
                                session
                                    .fail_pending_requests(
                                        &pending_ids,
                                        "originating POST disconnected before the client request was delivered",
                                    )
                                    .await;
                                return;
                            }
                        }
                        None => requests_open = false,
                    }
                }
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(event_rx);
    let mut response = Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(30))
                .text("ping"),
        )
        .into_response();
    response.headers_mut().insert(
        MCP_PROTOCOL_VERSION_HEADER,
        HeaderValue::from_str(&negotiated_version).unwrap(),
    );
    response
}

async fn send_associated_request(
    session: &Session,
    event_tx: &tokio::sync::mpsc::Sender<std::result::Result<Event, Infallible>>,
    outgoing: OutgoingRequest,
    pending_ids: &mut Vec<RequestId>,
) -> bool {
    let id = outgoing.id.clone();
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: id.clone(),
        method: outgoing.method,
        params: Some(outgoing.params),
    };
    let data = match serde_json::to_string(&request) {
        Ok(data) => data,
        Err(error) => {
            let _ = outgoing.response_tx.send(Err(Error::Internal(format!(
                "Failed to serialize associated client request: {error}"
            ))));
            return true;
        }
    };

    session
        .add_pending_request(id.clone(), outgoing.response_tx)
        .await;
    pending_ids.push(id);

    event_tx
        .send(Ok(Event::default().event(SSE_MESSAGE_EVENT).data(data)))
        .await
        .is_ok()
}

/// Returns `true` when the given protocol version string enables `subscriptions/listen`.
///
/// `subscriptions/listen` is part of the 2026-07-28 spec (SEP-2575 / SEP-2567).
/// Unknown future dates do not opt into behavior that has not been compiled
/// and explicitly enabled.
fn version_supports_subscriptions_listen(
    version: &str,
    protocol_support: &ProtocolSupport,
) -> bool {
    version == PROTOCOL_VERSION_2026_07_28 && protocol_support.contains(version)
}

/// Serve a `subscriptions/listen` request as an SSE stream.
///
/// Subscribes to the session's notification broadcast channel and returns a
/// streaming `text/event-stream` response. The stream closes naturally when:
/// - The client disconnects (axum drops the response body).
/// - The broadcast channel closes (server shutdown / session expiry).
///
/// Each notification is assigned a monotonically increasing event ID for
/// potential stream resumption (SEP-1699).
async fn handle_subscriptions_listen_sse(session: Arc<Session>) -> Response {
    let rx = session.notifications_tx.subscribe();
    let session_clone = session.clone();

    let stream = BroadcastStream::new(rx)
        .then(move |result: std::result::Result<String, _>| {
            let session = session_clone.clone();
            async move {
                match result {
                    Ok(msg) => {
                        let event_id = session.next_event_id();
                        // Buffer the event for potential replay (SEP-1699)
                        session.buffer_event(event_id, msg.clone()).await;
                        Some(Ok::<_, Infallible>(
                            Event::default()
                                .id(event_id.to_string())
                                .event(SSE_MESSAGE_EVENT)
                                .data(msg),
                        ))
                    }
                    Err(_) => None,
                }
            }
        })
        .filter_map(|x| x);

    Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(30))
                .text("ping"),
        )
        .into_response()
}

/// Handle GET requests (SSE stream for server notifications and outgoing requests)
pub(super) async fn handle_get(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    let (parts, _body) = request.into_parts();
    let headers = parts.headers;
    let uri = parts.uri.clone();

    // Validate Host (DNS rebinding defense, complement to Origin)
    if let Some(resp) = validate_host(&headers, &uri, &state) {
        return resp;
    }

    // Validate Origin
    if let Some(resp) = validate_origin(&headers, &state) {
        return resp;
    }

    // Check Accept header
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !accept.contains("text/event-stream") {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "Accept header must include text/event-stream",
        )
            .into_response();
    }

    // Get session
    let session_id = match get_session_id(&headers) {
        Some(id) => id,
        None => {
            return json_rpc_error_response(None, JsonRpcError::session_required());
        }
    };

    let session = match state.sessions.get(&session_id).await {
        Some(s) => s,
        None => {
            return json_rpc_error_response(
                None,
                JsonRpcError::session_not_found_with_id(&session_id),
            );
        }
    };

    // Check for Last-Event-ID header for stream resumption (SEP-1699)
    let last_event_id = get_last_event_id(&headers);

    // GET is the resumable notification stream. Restricted server-to-client
    // requests are emitted only on their originating POST response stream.
    let rx = session.notifications_tx.subscribe();
    let session_clone = session.clone();

    // Replay buffered events if Last-Event-ID was provided (SEP-1699)
    let replay_events: Vec<_> = if let Some(after_id) = last_event_id {
        let events = session.get_events_after(after_id).await;
        tracing::debug!(
            after_id = after_id,
            replay_count = events.len(),
            "Replaying buffered events for stream resumption"
        );
        events
            .into_iter()
            .map(|e| {
                Ok::<_, Infallible>(
                    Event::default()
                        .id(e.id.to_string())
                        .event(SSE_MESSAGE_EVENT)
                        .data(e.data),
                )
            })
            .collect()
    } else {
        Vec::new()
    };

    // Create replay stream from buffered events
    let replay_stream = tokio_stream::iter(replay_events);

    // Create live stream for new events
    // Use `then` for async processing, then `filter_map` to remove errors
    let live_stream = BroadcastStream::new(rx)
        .then(move |result: std::result::Result<String, _>| {
            let session = session_clone.clone();
            async move {
                match result {
                    Ok(msg) => {
                        let event_id = session.next_event_id();
                        // Buffer the event for potential replay (SEP-1699)
                        session.buffer_event(event_id, msg.clone()).await;
                        Some(Ok::<_, Infallible>(
                            Event::default()
                                .id(event_id.to_string())
                                .event(SSE_MESSAGE_EVENT)
                                .data(msg),
                        ))
                    }
                    Err(_) => None,
                }
            }
        })
        .filter_map(|x| x);

    // Chain replay stream with live stream
    let stream = replay_stream.chain(live_stream);

    Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(30))
                .text("ping"),
        )
        .into_response()
}

/// Handle DELETE requests (session termination)
pub(super) async fn handle_delete(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    let (parts, _body) = request.into_parts();
    let headers = parts.headers;
    let uri = parts.uri.clone();

    // Validate Host (DNS rebinding defense, complement to Origin)
    if let Some(resp) = validate_host(&headers, &uri, &state) {
        return resp;
    }

    // Validate Origin
    if let Some(resp) = validate_origin(&headers, &state) {
        return resp;
    }

    let session_id = match get_session_id(&headers) {
        Some(id) => id,
        None => {
            return json_rpc_error_response(None, JsonRpcError::session_required());
        }
    };

    if state.sessions.remove(&session_id).await {
        tracing::info!(session_id = %session_id, "Session terminated");
        StatusCode::OK.into_response()
    } else {
        // For DELETE, it's okay if the session doesn't exist - it's already gone
        // Return OK instead of an error for idempotency
        tracing::debug!(session_id = %session_id, "Session already removed or never existed");
        StatusCode::OK.into_response()
    }
}

/// Handle GET /health requests
///
/// Returns a simple 200 OK response for health checks.
/// Does not require authentication or session state.
pub(super) async fn handle_health() -> Response {
    StatusCode::OK.into_response()
}

/// Build a synchronous JSON-RPC response wrapped in SSE format.
///
/// Used when [`AppState::sse_responses`] is `true`. The body is a single SSE
/// event followed by the required blank line:
///
/// ```text
/// event: message
/// data: <json>
///
/// ```
fn sse_json_response(response: impl serde::Serialize) -> Response {
    let json = match serde_json::to_string(&response) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "Failed to serialize response for SSE wrapping");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let sse_body = format!("event: message\ndata: {json}\n\n");
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        sse_body,
    )
        .into_response()
}

/// Create a JSON-RPC error response
pub(super) fn json_rpc_error_response(
    id: Option<crate::protocol::RequestId>,
    error: JsonRpcError,
) -> Response {
    let response = JsonRpcResponse::error(id, error);
    axum::Json(response).into_response()
}

pub(super) fn json_rpc_error_response_with_status(
    id: Option<crate::protocol::RequestId>,
    error: JsonRpcError,
    status: StatusCode,
) -> Response {
    let mut response = json_rpc_error_response(id, error);
    *response.status_mut() = status;
    response
}

/// HTTP 413 response for a POST body exceeding [`HttpTransport::max_body_size`].
fn body_too_large_response(limit: usize) -> Response {
    let mut resp = json_rpc_error_response(
        None,
        JsonRpcError::invalid_request(format!(
            "Request body exceeds the maximum size of {} bytes",
            limit
        )),
    );
    *resp.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
    resp
}

/// Returns `true` when the body-read error was caused by exceeding the
/// configured length limit (as opposed to a transport-level I/O failure).
fn is_length_limit_error(err: &axum::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = source {
        if e.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        source = e.source();
    }
    false
}
