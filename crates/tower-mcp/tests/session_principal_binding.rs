//! #1515: HTTP sessions are bound to the principal that created them.
//!
//! A request carrying `MCP-Session-Id` used to reach that session by ID
//! alone. These tests drive the public transport with a layer that stands in
//! for authentication (an identity read from a header) and check that a
//! second principal cannot use, stream, or terminate someone else's session,
//! and that every refusal is indistinguishable from an ID the server has
//! never seen.

#![cfg(feature = "http")]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use tower::ServiceExt;
use tower_mcp::session_store::{MemorySessionStore, SessionStore};
use tower_mcp::{Extensions, HttpTransport, McpRouter, SessionHandle};

/// What the stand-in authentication layer resolves a caller into.
#[derive(Debug, Clone)]
struct Identity(String);

const SESSION_HEADER: &str = "mcp-session-id";

/// Maps the `x-identity` header to an [`Identity`] extension.
async fn attach_identity(mut request: Request<Body>, next: axum::middleware::Next) -> Response {
    if let Some(identity) = request
        .headers()
        .get("x-identity")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
    {
        request.extensions_mut().insert(Identity(identity));
    }
    next.run(request).await
}

fn test_router() -> McpRouter {
    McpRouter::new().server_info("binding-test", "1.0.0")
}

/// A transport that resolves the principal from [`Identity`].
fn bound_transport() -> HttpTransport {
    HttpTransport::new(test_router())
        .disable_origin_validation()
        .bridge_extension::<Identity>()
        .session_principal_from_extension::<Identity>(|identity| identity.0.clone())
}

fn app(transport: HttpTransport) -> (Router, SessionHandle) {
    let (router, handle) = transport.into_router_with_handle();
    (
        router.layer(axum::middleware::from_fn(attach_identity)),
        handle,
    )
}

fn request(
    method: &str,
    identity: Option<&str>,
    session_id: Option<&str>,
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let accept = if method == "GET" {
        "text/event-stream"
    } else {
        "application/json, text/event-stream"
    };
    let mut builder = Request::builder()
        .method(method)
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", accept);
    if let Some(identity) = identity {
        builder = builder.header("x-identity", identity);
    }
    if let Some(session_id) = session_id {
        builder = builder.header(SESSION_HEADER, session_id);
    }
    builder
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

fn initialize_body() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "test-client", "version": "1.0.0"}
        }
    })
}

fn tools_list_body() -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
}

/// Initialize as `identity` and complete the handshake. Returns the session ID.
async fn open_session(app: &Router, identity: Option<&str>) -> String {
    let response = app
        .clone()
        .oneshot(request("POST", identity, None, Some(initialize_body())))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let session_id = response
        .headers()
        .get(SESSION_HEADER)
        .expect("initialize returns a session id")
        .to_str()
        .unwrap()
        .to_string();

    let response = app
        .clone()
        .oneshot(request(
            "POST",
            identity,
            Some(&session_id),
            Some(serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            })),
        ))
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "initialized notification: {}",
        response.status()
    );
    session_id
}

async fn body_json(response: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// `tools/list` for `session_id`, returning status and JSON body.
async fn list(
    app: &Router,
    identity: Option<&str>,
    session_id: &str,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(request(
            "POST",
            identity,
            Some(session_id),
            Some(tools_list_body()),
        ))
        .await
        .unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

/// The status of a GET SSE stream request. The body is not read, since a
/// successful stream never ends.
async fn stream_status(
    app: &Router,
    identity: Option<&str>,
    session_id: &str,
) -> (StatusCode, Option<serde_json::Value>) {
    let response = app
        .clone()
        .oneshot(request("GET", identity, Some(session_id), None))
        .await
        .unwrap();
    let status = response.status();
    let is_sse = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    if is_sse {
        (status, None)
    } else {
        (status, Some(body_json(response).await))
    }
}

async fn delete(
    app: &Router,
    identity: Option<&str>,
    session_id: &str,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(request("DELETE", identity, Some(session_id), None))
        .await
        .unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

/// What a server with no such session answers, for the same ID. Built from a
/// separate transport that never created anything.
struct Unknown {
    post: (StatusCode, serde_json::Value),
    get: (StatusCode, Option<serde_json::Value>),
    delete: (StatusCode, serde_json::Value),
}

async fn unknown_responses(session_id: &str) -> Unknown {
    let (app, _) = app(bound_transport());
    Unknown {
        post: list(&app, Some("alice"), session_id).await,
        get: stream_status(&app, Some("alice"), session_id).await,
        delete: delete(&app, Some("alice"), session_id).await,
    }
}

/// 1. The creating principal keeps full use of its session.
#[tokio::test]
async fn same_principal_can_post_stream_and_delete() {
    let (app, handle) = app(bound_transport());
    let session_id = open_session(&app, Some("alice")).await;

    let (status, json) = list(&app, Some("alice"), &session_id).await;
    assert_eq!(status, StatusCode::OK);
    assert!(json.get("result").is_some(), "expected a result: {json}");

    let (status, body) = stream_status(&app, Some("alice"), &session_id).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_none(), "expected an SSE stream");

    let (status, _) = delete(&app, Some("alice"), &session_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(handle.session_count().await, 0, "alice's DELETE removes it");
}

/// 2. A different principal is answered exactly as for an unknown ID, on
/// every method, and cannot terminate the session.
#[tokio::test]
async fn different_principal_is_answered_as_an_unknown_session() {
    let (app, handle) = app(bound_transport());
    let session_id = open_session(&app, Some("alice")).await;
    let unknown = unknown_responses(&session_id).await;
    assert_eq!(unknown.post.1["error"]["code"], -32005);

    assert_eq!(list(&app, Some("bob"), &session_id).await, unknown.post);
    assert_eq!(
        stream_status(&app, Some("bob"), &session_id).await,
        unknown.get
    );
    assert_eq!(delete(&app, Some("bob"), &session_id).await, unknown.delete);

    // The refused DELETE left the session in place for its owner.
    assert_eq!(handle.session_count().await, 1);
    let (status, json) = list(&app, Some("alice"), &session_id).await;
    assert_eq!(status, StatusCode::OK);
    assert!(json.get("result").is_some(), "expected a result: {json}");
}

/// 3. Dropping the credential does not grant access to a bound session.
#[tokio::test]
async fn request_without_a_principal_cannot_use_a_bound_session() {
    let (app, handle) = app(bound_transport());
    let session_id = open_session(&app, Some("alice")).await;
    let unknown = unknown_responses(&session_id).await;

    assert_eq!(list(&app, None, &session_id).await, unknown.post);
    assert_eq!(stream_status(&app, None, &session_id).await, unknown.get);
    assert_eq!(delete(&app, None, &session_id).await, unknown.delete);
    assert_eq!(handle.session_count().await, 1);
}

/// The reverse also holds: a session created anonymously is not usable by a
/// caller that now presents a principal.
#[tokio::test]
async fn anonymous_session_is_not_usable_by_an_authenticated_request() {
    let (app, _) = app(bound_transport());
    let session_id = open_session(&app, None).await;
    let unknown = unknown_responses(&session_id).await;

    assert_eq!(list(&app, Some("alice"), &session_id).await, unknown.post);
    let (status, json) = list(&app, None, &session_id).await;
    assert_eq!(status, StatusCode::OK);
    assert!(json.get("result").is_some(), "expected a result: {json}");
}

/// 4. With no authentication anywhere every request is anonymous and the
/// behavior is what it was before binding.
#[tokio::test]
async fn without_authentication_sessions_behave_as_before() {
    let transport = HttpTransport::new(test_router()).disable_origin_validation();
    let (router, handle) = transport.into_router_with_handle();
    let app = router;
    let session_id = open_session(&app, None).await;

    let (status, json) = list(&app, None, &session_id).await;
    assert_eq!(status, StatusCode::OK);
    assert!(json.get("result").is_some(), "expected a result: {json}");
    let (status, body) = stream_status(&app, None, &session_id).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_none());
    let (status, _) = delete(&app, None, &session_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(handle.session_count().await, 0);
}

/// 5a. A custom resolver decides the principal: two callers the layer tells
/// apart by identity but that the resolver maps to one key share a session.
#[tokio::test]
async fn custom_resolver_is_honored() {
    let transport = HttpTransport::new(test_router())
        .disable_origin_validation()
        .bridge_extension::<Identity>()
        .session_principal_resolver(|extensions: &Extensions| {
            // Group by the part before the '@': "alice@laptop" and
            // "alice@phone" are the same principal.
            extensions
                .get::<Identity>()
                .map(|identity| identity.0.split('@').next().unwrap_or_default().to_string())
        });
    let (app, _) = app(transport);
    let session_id = open_session(&app, Some("alice@laptop")).await;

    let (status, json) = list(&app, Some("alice@phone"), &session_id).await;
    assert_eq!(status, StatusCode::OK);
    assert!(json.get("result").is_some(), "expected a result: {json}");

    let (_, json) = list(&app, Some("bob@laptop"), &session_id).await;
    assert_eq!(json["error"]["code"], -32005);
}

/// 5b. A resolver that returns an empty principal denies. An `initialize`
/// is rejected and creates no session.
#[tokio::test]
async fn empty_principal_denies_initialize() {
    let transport = HttpTransport::new(test_router())
        .disable_origin_validation()
        .session_principal_resolver(|_: &Extensions| Some("  ".to_string()));
    let (app, handle) = app(transport);

    let response = app
        .clone()
        .oneshot(request("POST", None, None, Some(initialize_body())))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(response.headers().get(SESSION_HEADER).is_none());
    assert_eq!(handle.session_count().await, 0);
}

/// 5c. A resolver that panics denies, and does not take the server down.
#[tokio::test]
async fn panicking_principal_resolver_denies_initialize() {
    let transport = HttpTransport::new(test_router())
        .disable_origin_validation()
        .session_principal_resolver(|_: &Extensions| -> Option<String> { panic!("resolver bug") });
    let (app, handle) = app(transport);

    let response = app
        .clone()
        .oneshot(request("POST", None, None, Some(initialize_body())))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(handle.session_count().await, 0);
}

/// 5d. A resolver that fails on a later request is an invalid resolution,
/// which never matches, not even the anonymous session it might collapse to.
#[tokio::test]
async fn invalid_resolution_never_matches_an_existing_session() {
    // Anonymous unless the request says `x-identity: broken`, in which case
    // the resolver returns an empty principal.
    let transport = HttpTransport::new(test_router())
        .disable_origin_validation()
        .bridge_extension::<Identity>()
        .session_principal_resolver(|extensions: &Extensions| {
            extensions.get::<Identity>().map(|identity| {
                if identity.0 == "broken" {
                    String::new()
                } else {
                    identity.0.clone()
                }
            })
        });
    let (app, handle) = app(transport);
    let anonymous = open_session(&app, None).await;
    let unknown = unknown_responses(&anonymous).await;

    assert_eq!(list(&app, Some("broken"), &anonymous).await, unknown.post);
    assert_eq!(
        delete(&app, Some("broken"), &anonymous).await,
        unknown.delete
    );
    assert_eq!(handle.session_count().await, 1);
}

/// 6. The principal is written to the session store and a session restored
/// on another instance keeps enforcing it.
#[tokio::test]
async fn principal_survives_the_session_store_and_is_enforced_after_restore() {
    let store = Arc::new(MemorySessionStore::new());
    let shared: Arc<dyn SessionStore> = store.clone();

    let (instance_a, _) = app(bound_transport().session_store(shared.clone()));
    let session_id = open_session(&instance_a, Some("alice")).await;

    let record = store
        .load(&session_id)
        .await
        .unwrap()
        .expect("the session is persisted");
    assert_eq!(record.principal.as_deref(), Some("alice"));

    // A second instance has no live session and rebuilds it from the record.
    let (instance_b, handle_b) = app(bound_transport().session_store(shared.clone()));

    // Bob is refused before anything is restored.
    let unknown = unknown_responses(&session_id).await;
    assert_eq!(
        list(&instance_b, Some("bob"), &session_id).await,
        unknown.post
    );
    assert_eq!(
        handle_b.session_count().await,
        0,
        "a refused request must not restore the session"
    );

    // Alice is served, and the restored session still carries her principal.
    let (status, json) = list(&instance_b, Some("alice"), &session_id).await;
    assert_eq!(status, StatusCode::OK);
    assert!(json.get("result").is_some(), "expected a result: {json}");
    assert_eq!(handle_b.session_count().await, 1);
    assert_eq!(
        list(&instance_b, Some("bob"), &session_id).await,
        unknown.post
    );
    let record = store.load(&session_id).await.unwrap().unwrap();
    assert_eq!(record.principal.as_deref(), Some("alice"));
}

/// Auto-reinitialization has no record to take a principal from, so the
/// caller that recreates the session becomes its principal.
#[tokio::test]
async fn auto_reinitialized_session_is_bound_to_the_recreating_caller() {
    let (app, _) = app(bound_transport().auto_reinitialize_sessions(true));

    let (status, json) = list(&app, Some("alice"), "client-made-up-id").await;
    assert_eq!(status, StatusCode::OK);
    assert!(json.get("result").is_some(), "expected a result: {json}");

    let (_, json) = list(&app, Some("bob"), "client-made-up-id").await;
    assert_eq!(json["error"]["code"], -32005);
    let (status, json) = list(&app, Some("alice"), "client-made-up-id").await;
    assert_eq!(status, StatusCode::OK);
    assert!(json.get("result").is_some(), "expected a result: {json}");
}

/// The default resolver is the OAuth subject, verbatim, the same mapping the
/// router uses for Task owners.
#[cfg(feature = "oauth")]
mod oauth_default {
    use super::*;
    use tower_mcp::oauth::token::TokenClaims;

    async fn attach_claims(mut request: Request<Body>, next: axum::middleware::Next) -> Response {
        if let Some(sub) = request
            .headers()
            .get("x-sub")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
        {
            request.extensions_mut().insert(TokenClaims {
                sub: Some(sub),
                iss: Some("https://issuer.example".into()),
                aud: None,
                exp: None,
                scope: None,
                client_id: None,
                extra: std::collections::HashMap::new(),
            });
        }
        next.run(request).await
    }

    fn with_sub(mut request: Request<Body>, sub: Option<&str>) -> Request<Body> {
        if let Some(sub) = sub {
            request.headers_mut().insert("x-sub", sub.parse().unwrap());
        }
        request
    }

    #[tokio::test]
    async fn default_resolver_binds_sessions_to_the_oauth_subject() {
        let router = HttpTransport::new(test_router())
            .disable_origin_validation()
            .into_router()
            .layer(axum::middleware::from_fn(attach_claims));

        let response = router
            .clone()
            .oneshot(with_sub(
                request("POST", None, None, Some(initialize_body())),
                Some("alice"),
            ))
            .await
            .unwrap();
        let session_id = response
            .headers()
            .get(SESSION_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        router
            .clone()
            .oneshot(with_sub(
                request(
                    "POST",
                    None,
                    Some(&session_id),
                    Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/initialized"
                    })),
                ),
                Some("alice"),
            ))
            .await
            .unwrap();

        let call = |sub: Option<&'static str>| {
            let router = router.clone();
            let session_id = session_id.clone();
            async move {
                let response = router
                    .oneshot(with_sub(
                        request("POST", None, Some(&session_id), Some(tools_list_body())),
                        sub,
                    ))
                    .await
                    .unwrap();
                body_json(response).await
            }
        };
        assert!(call(Some("alice")).await.get("result").is_some());
        assert_eq!(call(Some("bob")).await["error"]["code"], -32005);
        assert_eq!(call(None).await["error"]["code"], -32005);
    }
}
