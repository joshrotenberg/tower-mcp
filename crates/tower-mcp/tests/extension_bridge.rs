//! #1242: bridging server-supplied request extensions into `RequestContext`.
//!
//! A tower layer in front of the transport can attach anything it likes to
//! the request. Before this, only OAuth `TokenClaims` was lifted across, so
//! `ctx.extension::<T>()` never found what a layer had inserted.
//!
//! These tests drive the reporter's actual shape: a per-request secret in a
//! header, mapped to an identity by a layer, read by a tool handler.

#![cfg(any(feature = "http", feature = "websocket"))]

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::Request;
use axum::response::Response;
#[cfg(feature = "http")]
use tower::ServiceExt;
#[cfg(feature = "http")]
use tower_mcp::HttpTransport;
use tower_mcp::extract::{Context, RawArgs, State};
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

/// What a layer resolves a caller's secret into. The reporter's case: the
/// server cannot otherwise tell one loopback caller from another.
#[derive(Debug, Clone, PartialEq)]
struct AgentIdentity(String);

/// A second type, registered nowhere, to prove bridging is per type.
#[derive(Debug, Clone, PartialEq)]
struct NeverRegistered(u32);

#[derive(Default, Clone)]
struct Seen(Arc<Mutex<Option<String>>>);

fn router(seen: Seen) -> McpRouter {
    let tool = ToolBuilder::new("whoami")
        .description("Reports the identity a layer attached, if any.")
        .read_only()
        .extractor_handler(
            seen,
            |State(seen): State<Seen>, ctx: Context, RawArgs(_): RawArgs| async move {
                let identity = ctx.extension::<AgentIdentity>().map(|i| i.0.clone());
                let leaked = ctx.extension::<NeverRegistered>().is_some();
                *seen.0.lock().unwrap() = identity.clone();
                Ok(CallToolResult::text(format!(
                    "{}|leaked={leaked}",
                    identity.unwrap_or_else(|| "anonymous".into())
                )))
            },
        )
        .build();
    McpRouter::new()
        .server_info("bridge-test", "1.0.0")
        .tool(tool)
}

/// Stands in for the reporter's layer: maps a per-agent secret in a header
/// to an identity. Also inserts a type nobody registered.
async fn attach_identity(mut request: Request<Body>, next: axum::middleware::Next) -> Response {
    if let Some(secret) = request
        .headers()
        .get("x-agent-secret")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
    {
        request.extensions_mut().insert(AgentIdentity(secret));
    }
    request.extensions_mut().insert(NeverRegistered(1));
    next.run(request).await
}

#[cfg(feature = "http")]
/// Call `whoami` over HTTP, optionally registering the bridge and optionally
/// sending the secret header. Returns the handler's text answer.
async fn call(register_bridge: bool, secret: Option<&str>) -> (String, Seen) {
    let seen = Seen::default();
    let transport = HttpTransport::new(router(seen.clone())).disable_origin_validation();
    let transport = if register_bridge {
        transport.bridge_extension::<AgentIdentity>()
    } else {
        transport
    };
    let app = transport
        .into_router()
        .layer(axum::middleware::from_fn(attach_identity));

    let mut request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json");
    if let Some(secret) = secret {
        request = request.header("x-agent-secret", secret);
    }
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "whoami", "arguments": {}}
    });
    let response = app
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "expected 2xx, got {}",
        response.status()
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let text = json["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no tool text in {json}"))
        .to_string();
    (text, seen)
}

/// The core claim: a registered type attached by a layer reaches the handler.
#[cfg(feature = "http")]
#[tokio::test]
async fn a_registered_extension_reaches_the_handler() {
    let (text, seen) = call(true, Some("agent-7")).await;
    assert_eq!(text, "agent-7|leaked=false");
    assert_eq!(*seen.0.lock().unwrap(), Some("agent-7".to_string()));
}

/// Bridging is per type: the same layer inserts `NeverRegistered` on every
/// request and it must not cross, or the opt-in would be meaningless.
#[cfg(feature = "http")]
#[tokio::test]
async fn an_unregistered_extension_does_not_cross() {
    let (text, _) = call(true, Some("agent-7")).await;
    assert!(
        text.ends_with("|leaked=false"),
        "an unregistered type must not be bridged: {text}"
    );
}

/// Default behaviour is unchanged: with nothing registered, a layer's value
/// stays invisible exactly as before.
#[cfg(feature = "http")]
#[tokio::test]
async fn without_registration_nothing_is_bridged() {
    let (text, seen) = call(false, Some("agent-7")).await;
    assert_eq!(text, "anonymous|leaked=false");
    assert_eq!(*seen.0.lock().unwrap(), None);
}

/// A request the layer did not tag is not an error. A layer that only
/// attaches its value on some routes is a normal configuration.
#[cfg(feature = "http")]
#[tokio::test]
async fn a_request_without_the_extension_is_not_an_error() {
    let (text, seen) = call(true, None).await;
    assert_eq!(text, "anonymous|leaked=false");
    assert_eq!(*seen.0.lock().unwrap(), None);
}

// ============================================================================
// The other dispatch paths
// ============================================================================
//
// `Extensions` is built at five places in the HTTP transport. The tests above
// cover the plain POST; these cover the session handshake and the 2026-07-28
// header path, so a site left unwired shows up as a failure rather than as a
// gap nobody notices.

#[cfg(feature = "http")]
/// Build the app once so a session survives across requests.
fn app_with_bridge(seen: Seen) -> axum::Router {
    HttpTransport::new(router(seen))
        .disable_origin_validation()
        .bridge_extension::<AgentIdentity>()
        .into_router()
        .layer(axum::middleware::from_fn(attach_identity))
}

#[cfg(feature = "http")]
async fn post(
    app: &axum::Router,
    body: serde_json::Value,
    headers: &[(&str, &str)],
) -> (axum::http::HeaderMap, serde_json::Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let response_headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    // A session response may arrive as SSE; take the data line either way.
    let json = serde_json::from_str(&text).unwrap_or_else(|_| {
        // A notification is answered with an empty body; a session request
        // may be answered as SSE.
        match text.lines().find_map(|l| l.strip_prefix("data: ")) {
            Some(data) => serde_json::from_str(data).expect("SSE data is JSON"),
            None => serde_json::Value::Null,
        }
    });
    (response_headers, json)
}

/// The session path builds its extensions at a different site than the plain
/// POST, and it is the common production path.
#[cfg(feature = "http")]
#[tokio::test]
async fn a_registered_extension_reaches_the_handler_on_a_session() {
    let seen = Seen::default();
    let app = app_with_bridge(seen.clone());

    let (headers, init) = post(
        &app,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "bridge-test", "version": "1.0.0"}
            }
        }),
        &[("x-agent-secret", "agent-9")],
    )
    .await;
    assert!(init["result"].is_object(), "initialize failed: {init}");
    let session = headers
        .get("mcp-session-id")
        .expect("session id")
        .to_str()
        .unwrap()
        .to_string();

    post(
        &app,
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        &[("x-agent-secret", "agent-9"), ("mcp-session-id", &session)],
    )
    .await;

    let (_, response) = post(
        &app,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "whoami", "arguments": {}}
        }),
        &[("x-agent-secret", "agent-9"), ("mcp-session-id", &session)],
    )
    .await;
    assert_eq!(
        response["result"]["content"][0]["text"], "agent-9|leaked=false",
        "session path did not bridge: {response}"
    );
    assert_eq!(*seen.0.lock().unwrap(), Some("agent-9".to_string()));
}

/// The 2026-07-28 path dispatches on the version header and skips the
/// handshake entirely, so it builds its extensions at yet another site.
#[cfg(feature = "stateless")]
#[tokio::test]
async fn a_registered_extension_reaches_the_handler_on_the_final_protocol() {
    let seen = Seen::default();
    let app = app_with_bridge(seen.clone());
    let version = tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28;

    let (_, response) = post(
        &app,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {
                "name": "whoami",
                "arguments": {},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": version,
                    "io.modelcontextprotocol/clientCapabilities": {},
                    "io.modelcontextprotocol/clientInfo": {
                        "name": "bridge-test", "version": "1.0.0"
                    }
                }
            }
        }),
        &[
            ("x-agent-secret", "agent-final"),
            ("Mcp-Protocol-Version", version),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "whoami"),
        ],
    )
    .await;
    assert_eq!(
        response["result"]["content"][0]["text"], "agent-final|leaked=false",
        "final protocol path did not bridge: {response}"
    );
    assert_eq!(*seen.0.lock().unwrap(), Some("agent-final".to_string()));
}

// ============================================================================
// WebSocket (#1242)
// ============================================================================
//
// The WebSocket transport had the identical TokenClaims-only lift. The value
// is read once from the HTTP request that opens the socket, so it applies to
// every request on that connection.

#[cfg(feature = "websocket")]
mod websocket {
    use super::{AgentIdentity, NeverRegistered, Seen, attach_identity, router};
    use std::time::Duration;
    use tower_mcp::client::{McpClient, WebSocketClientConfig, WebSocketClientTransport};

    async fn serve(seen: Seen, register_bridge: bool) -> String {
        let transport = tower_mcp::WebSocketTransport::new(router(seen));
        let transport = if register_bridge {
            transport.bridge_extension::<AgentIdentity>()
        } else {
            transport
        };
        let app = transport
            .into_router()
            .layer(axum::middleware::from_fn(attach_identity));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        format!("ws://{addr}/")
    }

    async fn whoami(url: &str, secret: Option<&str>) -> String {
        let mut config = WebSocketClientConfig::default();
        if let Some(secret) = secret {
            config.headers = vec![("x-agent-secret".to_string(), secret.to_string())];
        }
        let transport = WebSocketClientTransport::connect_with_config(url, config)
            .await
            .expect("connect");
        let client = McpClient::connect(transport).await.expect("client");
        client.initialize("ws-client", "1.0.0").await.expect("init");
        client
            .call_tool("whoami", serde_json::json!({}))
            .await
            .expect("call")
            .all_text()
    }

    #[tokio::test]
    async fn a_registered_extension_reaches_the_handler() {
        let seen = Seen::default();
        let url = serve(seen.clone(), true).await;
        assert_eq!(
            whoami(&url, Some("agent-ws")).await,
            "agent-ws|leaked=false"
        );
        assert_eq!(*seen.0.lock().unwrap(), Some("agent-ws".to_string()));
    }

    #[tokio::test]
    async fn without_registration_nothing_is_bridged() {
        let seen = Seen::default();
        let url = serve(seen.clone(), false).await;
        assert_eq!(
            whoami(&url, Some("agent-ws")).await,
            "anonymous|leaked=false"
        );
        assert_eq!(*seen.0.lock().unwrap(), None);
    }

    /// Keeps the unregistered marker referenced on this path too.
    #[tokio::test]
    async fn an_unregistered_extension_does_not_cross() {
        let seen = Seen::default();
        let url = serve(seen, true).await;
        let text = whoami(&url, Some("agent-ws")).await;
        assert!(
            text.ends_with("|leaked=false"),
            "an unregistered type must not be bridged: {text}"
        );
        let _ = NeverRegistered(0);
    }
}
