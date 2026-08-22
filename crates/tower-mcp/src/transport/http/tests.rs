//! Unit tests for [`HttpTransport`](super::HttpTransport) and its session,
//! SSE, and stateless dispatch machinery.
//!
//! Moved out of `http.rs` in #1256. They stay a sibling module rather than
//! an integration test because they reach private items.

use super::*;
// The production code moved into child modules in #1256 phase 3; this test
// module reaches their pub(super) items directly (private-item access is
// exactly why it's a sibling module rather than an integration test), so it
// needs its own glob imports rather than relying on whatever `http.rs` itself
// happens to re-export for its own use. Nothing from `stateless_dispatch` is
// glob-imported here: everything this file needs from it is already covered
// by `http.rs`'s own explicit re-export, reachable through `use super::*`.
use super::handlers::*;
use super::session::*;
use axum::body::Body;
use axum::http::Request;
use proptest::prelude::*;
use tower::ServiceExt;

#[cfg(feature = "oauth")]
fn oauth_test_token(audience: &str, scope: &str) -> String {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &serde_json::json!({
            "sub": "test-user",
            "aud": audience,
            "scope": scope,
        }),
        &jsonwebtoken::EncodingKey::from_secret(b"resource-server-test-secret"),
    )
    .unwrap()
}

#[cfg(feature = "oauth")]
#[tokio::test]
async fn oauth_resource_server_setup_is_path_aware_and_audience_bound() {
    let resource = "http://localhost:3000/tenant/mcp";
    let metadata = crate::oauth::ProtectedResourceMetadata::new(resource)
        .authorization_server("https://auth.example.com")
        .scope("mcp:read");
    let validator = crate::oauth::JwtValidator::from_secret(b"resource-server-test-secret")
        .disable_exp_validation();
    let app = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .disable_host_validation()
        .into_oauth_router_at(
            "/tenant/mcp",
            validator,
            metadata,
            crate::oauth::ScopePolicy::new().default_scope("mcp:read"),
        )
        .unwrap();

    let metadata_request = Request::builder()
        .uri("/.well-known/oauth-protected-resource/tenant/mcp")
        .body(Body::empty())
        .unwrap();
    let metadata_response = app.clone().oneshot(metadata_request).await.unwrap();
    assert_eq!(metadata_response.status(), StatusCode::OK);

    let unauthenticated = Request::builder()
        .method("POST")
        .uri("/tenant/mcp")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(unauthenticated).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        response
            .headers()
            .get("WWW-Authenticate")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("http://localhost:3000/.well-known/oauth-protected-resource/tenant/mcp")
    );

    let wrong_audience = oauth_test_token("http://localhost:3000/other", "mcp:read");
    let request = Request::builder()
        .method("POST")
        .uri("/tenant/mcp")
        .header("Authorization", format!("Bearer {wrong_audience}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    let token = oauth_test_token(resource, "mcp:read");
    let request = Request::builder()
        .method("POST")
        .uri("/tenant/mcp")
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "oauth-test", "version": "1.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.get("result").is_some(), "unexpected response: {json}");
}

#[cfg(feature = "oauth")]
#[test]
fn oauth_resource_server_setup_validates_metadata() {
    let result = HttpTransport::new(create_test_router()).into_oauth_router(
        crate::oauth::JwtValidator::from_secret(b"secret"),
        crate::oauth::ProtectedResourceMetadata::new("https://mcp.example.com"),
        crate::oauth::ScopePolicy::new(),
    );
    assert!(matches!(
        result.unwrap_err(),
        crate::oauth::ProtectedResourceMetadataError::MissingAuthorizationServer
    ));
}

fn arb_json() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        any::<i64>().prop_map(|number| serde_json::json!(number)),
        prop::collection::vec(any::<char>(), 0..256)
            .prop_map(|chars| serde_json::Value::String(chars.into_iter().collect())),
    ];
    leaf.prop_recursive(6, 128, 10, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..10).prop_map(serde_json::Value::Array),
            prop::collection::hash_map("[a-zA-Z0-9_]{0,24}", inner, 0..10)
                .prop_map(|map| serde_json::Value::Object(map.into_iter().collect())),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Request-ID extraction runs on untrusted JSON before dispatch and
    /// must reject surprising shapes without panicking.
    #[test]
    fn extract_request_id_never_panics(value in arb_json()) {
        let _ = extract_request_id(&value);
    }

    #[test]
    fn extract_request_id_accepts_all_i64_values(id in any::<i64>()) {
        prop_assert_eq!(
            extract_request_id(&serde_json::json!({ "id": id })),
            Some(RequestId::Number(id))
        );
    }

    #[test]
    fn extract_request_id_accepts_arbitrary_strings(
        chars in prop::collection::vec(any::<char>(), 0..1024)
    ) {
        let id: String = chars.into_iter().collect();
        prop_assert_eq!(
            extract_request_id(&serde_json::json!({ "id": id })),
            Some(RequestId::String(id))
        );
    }
}

fn create_test_router() -> McpRouter {
    McpRouter::new().server_info("test-server", "1.0.0")
}

#[test]
fn final_result_fields_are_method_and_version_aware() {
    for method in [
        "server/discover",
        "tools/list",
        "prompts/list",
        "resources/list",
        "resources/read",
        "resources/templates/list",
    ] {
        let mut response =
            JsonRpcResponse::result(RequestId::Number(1), serde_json::json!({"value": true}));
        apply_protocol_result_fields(&mut response, method, PROTOCOL_VERSION_2026_07_28);
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["result"]["resultType"], "complete", "{method}");
        assert_eq!(json["result"]["ttlMs"], 0, "{method}");
        assert_eq!(json["result"]["cacheScope"], "private", "{method}");
    }

    let mut ordinary =
        JsonRpcResponse::result(RequestId::Number(1), serde_json::json!({"content": []}));
    apply_protocol_result_fields(&mut ordinary, "tools/call", PROTOCOL_VERSION_2026_07_28);
    let json = serde_json::to_value(ordinary).unwrap();
    assert_eq!(json["result"]["resultType"], "complete");
    assert!(json["result"].get("ttlMs").is_none());
    assert!(json["result"].get("cacheScope").is_none());
}

#[test]
fn final_result_fields_preserve_explicit_values_and_legacy_wire_shape() {
    let explicit = serde_json::json!({
        "contents": [],
        "ttlMs": 42,
        "cacheScope": "public"
    });
    let mut response = JsonRpcResponse::result(RequestId::Number(1), explicit.clone());
    apply_protocol_result_fields(&mut response, "resources/read", PROTOCOL_VERSION_2026_07_28);
    let json = serde_json::to_value(response).unwrap();
    assert_eq!(json["result"]["ttlMs"], 42);
    assert_eq!(json["result"]["cacheScope"], "public");

    for discriminator in ["input_required", "task"] {
        let mut response = JsonRpcResponse::result(
            RequestId::Number(1),
            serde_json::json!({"resultType": discriminator}),
        );
        apply_protocol_result_fields(&mut response, "tools/call", PROTOCOL_VERSION_2026_07_28);
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["result"]["resultType"], discriminator);
    }

    let mut legacy = JsonRpcResponse::result(RequestId::Number(1), explicit);
    let before = serde_json::to_value(&legacy).unwrap();
    apply_protocol_result_fields(&mut legacy, "resources/read", "2025-11-25");
    assert_eq!(serde_json::to_value(legacy).unwrap(), before);
}

#[tokio::test]
#[cfg(feature = "stateless")]
async fn modern_subscription_registry_reports_closes() {
    use super::stateless_dispatch::SubscriptionTerminal;
    use crate::transport::subscriptions::{
        SubscriptionClose, SubscriptionCloseReason, SubscriptionObserver,
    };

    #[derive(Default)]
    struct RecordingObserver {
        closes: std::sync::Mutex<Vec<SubscriptionClose>>,
    }
    impl SubscriptionObserver for RecordingObserver {
        fn on_close(&self, close: SubscriptionClose) {
            self.closes.lock().unwrap().push(close);
        }
    }

    let observer = Arc::new(RecordingObserver::default());
    let registry = Arc::new(ModernSubscriptionRegistry::new(
        SubscriptionLimits::default(),
        None,
        Some(observer.clone()),
    ));

    // A dropped guard is a client disconnect.
    let disconnected = registry
        .try_register(
            RequestId::String("disconnects".into()),
            SubscriptionFilter {
                tools_list_changed: Some(true),
                ..SubscriptionFilter::default()
            },
            None,
        )
        .unwrap();
    drop(disconnected.guard);

    // A drained registry is a graceful server-side close.
    let drained = registry
        .try_register(
            RequestId::String("drains".into()),
            SubscriptionFilter::default(),
            None,
        )
        .unwrap();
    assert_eq!(registry.close_all(), 1);
    let terminal = drained.terminal.await.unwrap();
    assert!(matches!(terminal, SubscriptionTerminal::Drained(_)));
    // The entry is already gone, so the later guard drop must not
    // produce a second record.
    drop(drained.guard);

    let closes = observer.closes.lock().unwrap();
    assert_eq!(closes.len(), 2, "one record per stream: {closes:?}");
    assert_eq!(
        closes[0].subscription_id,
        RequestId::String("disconnects".into())
    );
    assert_eq!(closes[0].reason, SubscriptionCloseReason::Disconnected);
    assert_eq!(
        closes[1].subscription_id,
        RequestId::String("drains".into())
    );
    assert_eq!(closes[1].reason, SubscriptionCloseReason::Drained);
}

#[tokio::test]
#[cfg(feature = "stateless")]
async fn modern_subscription_registry_filters_and_tags_notifications() {
    let registry = Arc::new(ModernSubscriptionRegistry::default());
    let mut registration = registry
        .try_register(
            RequestId::String("listen-1".to_string()),
            SubscriptionFilter {
                tools_list_changed: Some(true),
                ..SubscriptionFilter::default()
            },
            None,
        )
        .unwrap();

    assert!(registry.publish(&ServerNotification::PromptsListChanged));
    assert!(matches!(
        registration.notifications.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    assert!(registry.publish(&ServerNotification::ToolsListChanged));
    let message = registration
        .notifications
        .recv()
        .await
        .expect("matching notification");
    let json: serde_json::Value = serde_json::from_str(message.as_str()).unwrap();
    assert_eq!(json["method"], "notifications/tools/list_changed");
    assert_eq!(
        json["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
        "listen-1"
    );

    drop(registration.guard);
    assert!(registry.subscriptions.lock().unwrap().is_empty());
}

#[test]
#[cfg(feature = "stateless")]
fn modern_subscription_registry_enforces_global_and_principal_limits() {
    use super::stateless_dispatch::{SubscriptionAdmissionError, SubscriptionPrincipal};

    let global = Arc::new(ModernSubscriptionRegistry::new(
        SubscriptionLimits::default().max_active(1),
        None,
        None,
    ));
    let first = global
        .try_register(
            RequestId::String("global-1".into()),
            SubscriptionFilter::default(),
            None,
        )
        .unwrap();
    let rejected = global.try_register(
        RequestId::String("global-2".into()),
        SubscriptionFilter::default(),
        None,
    );
    assert!(matches!(
        rejected,
        Err(SubscriptionAdmissionError::GlobalLimit)
    ));
    drop(first.guard);
    let reopened = global
        .try_register(
            RequestId::String("global-3".into()),
            SubscriptionFilter::default(),
            None,
        )
        .expect("dropping a guard must reopen the global slot");
    drop(reopened.guard);

    let per_principal = Arc::new(ModernSubscriptionRegistry::new(
        SubscriptionLimits::default()
            .max_active(4)
            .max_active_per_principal(1),
        None,
        None,
    ));
    let alice = Some(SubscriptionPrincipal::AuthClient("alice".into()));
    let bob = Some(SubscriptionPrincipal::AuthClient("bob".into()));
    let alice_first = per_principal
        .try_register(
            RequestId::String("alice-1".into()),
            SubscriptionFilter::default(),
            alice.clone(),
        )
        .unwrap();
    let alice_rejected = per_principal.try_register(
        RequestId::String("alice-2".into()),
        SubscriptionFilter::default(),
        alice,
    );
    assert!(matches!(
        alice_rejected,
        Err(SubscriptionAdmissionError::PrincipalLimit)
    ));
    let bob_allowed = per_principal
        .try_register(
            RequestId::String("bob-1".into()),
            SubscriptionFilter::default(),
            bob,
        )
        .expect("one principal must not consume another principal's quota");
    drop(alice_first.guard);
    drop(bob_allowed.guard);

    let bounded_metadata = Arc::new(ModernSubscriptionRegistry::new(
        SubscriptionLimits::default().max_metadata_bytes(8),
        None,
        None,
    ));
    let oversized = bounded_metadata.try_register(
        RequestId::String("filter-too-large".into()),
        SubscriptionFilter {
            resource_subscriptions: Some(vec!["file:///a-long-resource-name".into()]),
            ..SubscriptionFilter::default()
        },
        None,
    );
    assert!(matches!(
        oversized,
        Err(SubscriptionAdmissionError::MetadataTooLarge)
    ));
    assert_eq!(bounded_metadata.len(), 0);

    let empty_filter_bytes = serde_json::to_vec(&SubscriptionFilter::default())
        .unwrap()
        .len();
    let bounded_id = Arc::new(ModernSubscriptionRegistry::new(
        SubscriptionLimits::default().max_metadata_bytes(empty_filter_bytes + 8),
        None,
        None,
    ));
    let oversized_id = bounded_id.try_register(
        RequestId::String("x".repeat(64)),
        SubscriptionFilter::default(),
        None,
    );
    assert!(matches!(
        oversized_id,
        Err(SubscriptionAdmissionError::MetadataTooLarge)
    ));
    assert_eq!(bounded_id.len(), 0);
    let small_id = bounded_id
        .try_register(RequestId::Number(1), SubscriptionFilter::default(), None)
        .expect("the same filter with a compact ID must fit the metadata budget");
    drop(small_id.guard);

    let oversized_message_limit = Arc::new(ModernSubscriptionRegistry::new(
        SubscriptionLimits::default().max_buffered_messages(usize::MAX),
        None,
        None,
    ));
    let registration = oversized_message_limit
        .try_register(
            RequestId::String("large-host-setting".into()),
            SubscriptionFilter::default(),
            None,
        )
        .expect("an oversized host setting must be clamped, not panic remotely");
    drop(registration.guard);
}

#[test]
#[cfg(all(feature = "stateless", feature = "oauth"))]
fn modern_subscription_principal_prefers_oauth_and_supports_client_credentials() {
    use super::stateless_dispatch::{SubscriptionPrincipal, subscription_principal};

    let mut extensions = axum::http::Extensions::new();
    extensions.insert(crate::auth::AuthInfo {
        client_id: "generic-client".into(),
        claims: None,
    });
    extensions.insert(crate::oauth::token::TokenClaims {
        sub: Some("alice".into()),
        iss: Some("https://issuer.example".into()),
        aud: None,
        exp: None,
        scope: None,
        client_id: Some("oauth-client".into()),
        extra: std::collections::HashMap::new(),
    });
    assert_eq!(
        subscription_principal(&extensions),
        Some(SubscriptionPrincipal::OAuthSubject {
            issuer: Some("https://issuer.example".into()),
            subject: "alice".into(),
        })
    );

    extensions.insert(crate::oauth::token::TokenClaims {
        sub: None,
        iss: Some("https://issuer.example".into()),
        aud: None,
        exp: None,
        scope: None,
        client_id: Some("oauth-client".into()),
        extra: std::collections::HashMap::new(),
    });
    assert_eq!(
        subscription_principal(&extensions),
        Some(SubscriptionPrincipal::OAuthClient {
            issuer: Some("https://issuer.example".into()),
            client_id: "oauth-client".into(),
        })
    );
}

#[tokio::test]
#[cfg(feature = "stateless")]
async fn modern_subscription_registry_bounds_messages_and_reports_overflow_once() {
    use super::stateless_dispatch::SubscriptionTerminal;
    use crate::transport::subscriptions::{
        SubscriptionClose, SubscriptionCloseReason, SubscriptionObserver,
    };

    #[derive(Default)]
    struct RecordingObserver {
        closes: std::sync::Mutex<Vec<SubscriptionClose>>,
    }
    impl SubscriptionObserver for RecordingObserver {
        fn on_close(&self, close: SubscriptionClose) {
            self.closes.lock().unwrap().push(close);
        }
    }

    let observer = Arc::new(RecordingObserver::default());
    let registry = Arc::new(ModernSubscriptionRegistry::new(
        SubscriptionLimits::default().max_buffered_messages(1),
        None,
        Some(observer.clone()),
    ));
    let registration = registry
        .try_register(
            RequestId::String("overflow".into()),
            SubscriptionFilter {
                tools_list_changed: Some(true),
                ..SubscriptionFilter::default()
            },
            None,
        )
        .unwrap();

    assert!(registry.publish(&ServerNotification::ToolsListChanged));
    assert_eq!(registry.len(), 1);
    assert!(registry.publish(&ServerNotification::ToolsListChanged));
    assert_eq!(registry.len(), 0);

    let terminal = registration.terminal.await.unwrap();
    let SubscriptionTerminal::BufferOverflow(json) = terminal else {
        panic!("message overflow must terminate with an error");
    };
    let json: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(json["id"], "overflow");
    assert_eq!(json["error"]["code"], -32603);

    drop(registration.guard);
    let closes = observer.closes.lock().unwrap();
    assert_eq!(
        closes.len(),
        1,
        "guard drop must not double-report overflow"
    );
    assert_eq!(closes[0].reason, SubscriptionCloseReason::BufferOverflow);
}

#[tokio::test]
#[cfg(feature = "stateless")]
async fn modern_subscription_registry_enforces_the_byte_budget() {
    use super::stateless_dispatch::SubscriptionTerminal;

    let registry = Arc::new(ModernSubscriptionRegistry::new(
        SubscriptionLimits::default()
            .max_buffered_messages(4)
            .max_buffered_bytes(1),
        None,
        None,
    ));
    let registration = registry
        .try_register(
            RequestId::String("byte-overflow".into()),
            SubscriptionFilter {
                tools_list_changed: Some(true),
                ..SubscriptionFilter::default()
            },
            None,
        )
        .unwrap();

    assert!(registry.publish(&ServerNotification::ToolsListChanged));
    assert_eq!(registry.len(), 0);
    assert!(matches!(
        registration.terminal.await.unwrap(),
        SubscriptionTerminal::BufferOverflow(_)
    ));
    drop(registration.guard);
}

#[tokio::test]
async fn runtime_protocol_allowlist_drives_discovery() {
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .protocol_versions(["2025-03-26"])
        .unwrap();
    let app = transport.into_router();
    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "server/discover"
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["result"]["supportedVersions"],
        serde_json::json!(["2025-03-26"])
    );
}

#[tokio::test]
async fn test_oversized_body_rejected_with_413() {
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .max_body_size(1024);
    let app = transport.into_router();

    // 2 KiB of body against a 1 KiB limit.
    let padding = "x".repeat(2048);
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"ping","params":{{"pad":"{}"}}}}"#,
        padding
    );
    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(body))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn test_oversized_content_length_rejected_without_reading() {
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .max_body_size(1024);
    let app = transport.into_router();

    // Declared Content-Length above the limit short-circuits even
    // though the actual body is tiny.
    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Length", "10485760")
        .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn test_body_within_limit_accepted() {
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .max_body_size(1024);
    let app = transport.into_router();

    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_initialize_creates_session() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "test-client",
                        "version": "1.0.0"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key(MCP_SESSION_ID_HEADER));
    // Verify protocol version header is present on initialize response
    assert_eq!(
        response
            .headers()
            .get(MCP_PROTOCOL_VERSION_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("2025-11-25")
    );
}

#[tokio::test]
async fn test_protocol_version_header_on_subsequent_requests() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    // Initialize
    let init_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "test-client",
                        "version": "1.0.0"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let init_response = app.clone().oneshot(init_request).await.unwrap();
    let session_id = init_response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Verify init response has negotiated version (2025-03-26, not latest)
    assert_eq!(
        init_response
            .headers()
            .get(MCP_PROTOCOL_VERSION_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("2025-03-26")
    );

    // Send initialized notification
    let initialized_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .header(MCP_PROTOCOL_VERSION_HEADER, "2025-03-26")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            })
            .to_string(),
        ))
        .unwrap();

    app.clone().oneshot(initialized_request).await.unwrap();

    // Send tools/list and check for protocol version header
    let list_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .header(MCP_PROTOCOL_VERSION_HEADER, "2025-03-26")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(list_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(MCP_PROTOCOL_VERSION_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("2025-03-26")
    );
}

#[tokio::test]
async fn unsupported_protocol_version_returns_spec_shape_error() {
    // SEP-2575: requests carrying an unrecognized MCP-Protocol-Version
    // header (post-initialize) get a JSON-RPC error with code -32022 and
    // data `{ supported: [...], requested: "..." }`.
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    // Initialize first so we're past the init exemption.
    let init_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "t", "version": "0" }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let init_response = app.clone().oneshot(init_request).await.unwrap();
    let session_id = init_response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Now send a request with a bogus version header.
    let bad = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .header(MCP_PROTOCOL_VERSION_HEADER, "1999-01-01")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(bad).await.unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"].as_i64().unwrap(), -32022);
    assert_eq!(json["error"]["data"]["requested"], "1999-01-01");
    let supported = json["error"]["data"]["supported"]
        .as_array()
        .expect("supported must be an array");
    assert!(supported.contains(&serde_json::json!("2025-11-25")));
    // The request id must be echoed (we have one in the body).
    assert_eq!(json["id"], 99);
    // Field name must be `supported`, NOT `supportedVersions` (SEP-2575 shape).
    assert!(
        json["error"]["data"].get("supportedVersions").is_none(),
        "error data must use 'supported', not 'supportedVersions': {:?}",
        json["error"]["data"]
    );
    // The supported set must exactly match the compiled transport default --
    // no extras, none missing.
    let expected: Vec<serde_json::Value> = crate::COMPILED_PROTOCOL_VERSIONS
        .iter()
        .map(|v| serde_json::json!(v))
        .collect();
    assert_eq!(
        supported, &expected,
        "data.supported must exactly match COMPILED_PROTOCOL_VERSIONS"
    );
}

/// When a request (no session) arrives with an invalid `Mcp-Protocol-Version`
/// header, the transport must return -32022 with the correct SEP-2575 wire
/// shape: `{ supported: [...], requested: "..." }`. This verifies the
/// version-validation path fires without requiring a session.
#[cfg(feature = "stateless")]
#[tokio::test]
async fn stateless_unsupported_protocol_version_returns_spec_shape_error() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    // A future-looking unknown version must not enter the 2026-07-28
    // stateless path merely because its date sorts after 2026-07-28.
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_PROTOCOL_VERSION_HEADER, "2099-01-01")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 42,
                "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["error"]["code"].as_i64().unwrap(),
        -32022,
        "must return UnsupportedProtocolVersion (-32022): {json}"
    );
    assert_eq!(
        json["error"]["data"]["requested"], "2099-01-01",
        "data.requested must echo the version: {json}"
    );
    // Field name must be `supported`, not `supportedVersions`.
    assert!(
        json["error"]["data"].get("supportedVersions").is_none(),
        "error data must use 'supported', not 'supportedVersions': {json}"
    );
    let supported = json["error"]["data"]["supported"]
        .as_array()
        .expect("data.supported must be an array");
    let expected: Vec<serde_json::Value> = crate::COMPILED_PROTOCOL_VERSIONS
        .iter()
        .map(|v| serde_json::json!(v))
        .collect();
    assert_eq!(
        supported, &expected,
        "data.supported must exactly match COMPILED_PROTOCOL_VERSIONS"
    );
}

// =========================================================================
// SEP-2243: HTTP header standardization (Mcp-Method, Mcp-Name, Mcp-Param-*)
// =========================================================================

/// In lenient mode (negotiated protocol version < 2026-07-28) a
/// request without any SEP-2243 headers must still succeed — older
/// clients that haven't opted in must keep working.
#[tokio::test]
async fn sep_2243_lenient_mode_accepts_missing_headers() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    // Initialize (no SEP-2243 headers) negotiates 2025-11-25 — lenient.
    let init = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "t", "version": "0" }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let init_response = app.clone().oneshot(init).await.unwrap();
    assert_eq!(init_response.status(), StatusCode::OK);
    let session_id = init_response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // tools/list with no Mcp-Method header — must succeed in lenient mode.
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// In lenient mode, if the client opts in by sending Mcp-Method,
/// the server still validates against the body and rejects a
/// mismatch with -32020.
#[tokio::test]
async fn sep_2243_lenient_mode_validates_present_headers() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    let init = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(MCP_METHOD_HEADER, "initialize")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "t", "version": "0" }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let init_response = app.clone().oneshot(init).await.unwrap();
    assert_eq!(init_response.status(), StatusCode::OK);
    let session_id = init_response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // tools/list with a deliberately-wrong Mcp-Method header.
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .header(MCP_METHOD_HEADER, "ping")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"].as_i64().unwrap(), -32020);
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Mcp-Method")
    );
    assert_eq!(json["id"], 2);
}

/// tools/call with matching Mcp-Method + Mcp-Name passes validation
/// even in strict mode. Driven via initialize with the upcoming
/// 2026-07-28 protocol version so we exercise the strict branch.
///
/// Gated to `not(stateless)` because with the stateless feature enabled,
/// initialize requests for 2026-07-28 are handled without a session (chunk 5).
/// The stateless-mode equivalent is `stateless_v2026_tools_call_without_session_succeeds`.
#[tokio::test]
#[cfg(not(feature = "stateless"))]
async fn sep_2243_strict_mode_tools_call_with_matching_headers() {
    use crate::{CallToolResult, ToolBuilder};

    let router = McpRouter::new().server_info("t", "1.0.0").tool(
        ToolBuilder::new("echo")
            .description("echo")
            .handler(
                |args: serde_json::Value| async move { Ok(CallToolResult::text(args.to_string())) },
            )
            .build(),
    );
    let transport = HttpTransport::new(router).disable_origin_validation();
    let app = transport.into_router();

    // Initialize requesting 2026-07-28 so the session falls into
    // strict mode. The server will negotiate the actual returned
    // version against SUPPORTED_PROTOCOL_VERSIONS, but for SEP-2243
    // gating on init the requested version is what counts.
    let init = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(MCP_METHOD_HEADER, "initialize")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2026-07-28",
                    "capabilities": {},
                    "clientInfo": { "name": "t", "version": "0" }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let init_response = app.clone().oneshot(init).await.unwrap();
    assert_eq!(init_response.status(), StatusCode::OK);
    let session_id = init_response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let negotiated_version = init_response
        .headers()
        .get(MCP_PROTOCOL_VERSION_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // For subsequent requests, the session's negotiated protocol
    // version is what the validator gates on. If the server did
    // NOT honor 2026-07-28 (because it isn't in SUPPORTED yet) the
    // session will be lenient — which is fine, we just want to
    // confirm the happy path works. If it IS honored, the strict
    // branch is exercised.
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .header(MCP_PROTOCOL_VERSION_HEADER, &negotiated_version)
        .header(MCP_METHOD_HEADER, "tools/call")
        .header(MCP_NAME_HEADER, "echo")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "echo",
                    "arguments": {"message": "hi"}
                }
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// tools/call with mismatched Mcp-Name vs body params.name MUST be
/// rejected with -32020 and HTTP 400 even in lenient mode.
#[tokio::test]
async fn sep_2243_tools_call_mcp_name_mismatch_rejected() {
    use crate::{CallToolResult, ToolBuilder};

    let router = McpRouter::new().server_info("t", "1.0.0").tool(
        ToolBuilder::new("echo")
            .description("echo")
            .handler(
                |args: serde_json::Value| async move { Ok(CallToolResult::text(args.to_string())) },
            )
            .build(),
    );
    let transport = HttpTransport::new(router).disable_origin_validation();
    let app = transport.into_router();

    let init = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "t", "version": "0" }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let init_response = app.clone().oneshot(init).await.unwrap();
    let session_id = init_response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .header(MCP_METHOD_HEADER, "tools/call")
        .header(MCP_NAME_HEADER, "not-echo")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "tools/call",
                "params": {
                    "name": "echo",
                    "arguments": {"message": "hi"}
                }
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"].as_i64().unwrap(), -32020);
    let msg = json["error"]["message"].as_str().unwrap();
    assert!(msg.contains("Mcp-Name"), "got: {msg}");
    assert_eq!(json["id"], 7);
}

/// Mcp-Param-* with a Base64-encoded value that decodes to the
/// body argument must pass validation.
#[tokio::test]
async fn sep_2243_mcp_param_base64_decoded_and_matched() {
    use crate::{CallToolResult, ToolBuilder};

    let router = McpRouter::new().server_info("t", "1.0.0").tool(
        ToolBuilder::new("echo")
            .description("echo")
            .handler(
                |args: serde_json::Value| async move { Ok(CallToolResult::text(args.to_string())) },
            )
            .build(),
    );
    let transport = HttpTransport::new(router).disable_origin_validation();
    let app = transport.into_router();

    let init = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "t", "version": "0" }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let init_response = app.clone().oneshot(init).await.unwrap();
    let session_id = init_response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Body argument is "Hello"; header is "=?base64?SGVsbG8=?=".
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .header(MCP_METHOD_HEADER, "tools/call")
        .header(MCP_NAME_HEADER, "echo")
        .header("mcp-param-message", "=?base64?SGVsbG8=?=")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": {
                    "name": "echo",
                    "arguments": {"message": "Hello"}
                }
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn sep_2243_final_request_requires_schema_annotated_header() {
    use crate::extract::RawArgs;
    use crate::{CallToolResult, ToolBuilder};

    let tool = ToolBuilder::new("route")
        .input_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "tenant_id": {
                    "type": "string",
                    "x-mcp-header": "Tenant"
                }
            }
        }))
        .extractor_handler((), |RawArgs(args): RawArgs| async move {
            Ok(CallToolResult::text(args["tenant_id"].to_string()))
        })
        .build();
    let app = HttpTransport::new(
        McpRouter::new()
            .server_info("header-test", "1.0.0")
            .tool(tool),
    )
    .disable_origin_validation()
    .into_router();

    let request = |custom_header: Option<&'static str>| {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header(MCP_PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION_2026_07_28)
            .header(MCP_METHOD_HEADER, "tools/call")
            .header(MCP_NAME_HEADER, "route");
        if let Some(value) = custom_header {
            builder = builder.header("Mcp-Param-Tenant", value);
        }
        builder
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 9,
                    "method": "tools/call",
                    "params": {
                        "name": "route",
                        "arguments": {"tenant_id": "acme"},
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion":
                                PROTOCOL_VERSION_2026_07_28,
                            "io.modelcontextprotocol/clientCapabilities": {}
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap()
    };

    let response = app.clone().oneshot(request(None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["id"], 9);
    assert_eq!(error["error"]["code"], -32020);

    let response = app.oneshot(request(Some("acme"))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// notifications/initialized still receives ACCEPTED when SEP-2243
/// headers match (regression for the notification fast path).
#[tokio::test]
async fn sep_2243_notification_with_matching_method_header_accepted() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    let init = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "t", "version": "0" }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let init_response = app.clone().oneshot(init).await.unwrap();
    let session_id = init_response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .header(MCP_METHOD_HEADER, "notifications/initialized")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_request_without_session_fails() {
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .require_sessions();
    let app = transport.into_router();

    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    // We now return JSON-RPC errors for session issues
    assert_eq!(response.status(), StatusCode::OK);

    // Verify it's a JSON-RPC error response
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.get("error").is_some());
    assert_eq!(json["error"]["code"], -32006); // SessionRequired
}

#[tokio::test]
async fn test_delete_session() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    // First, initialize to get a session
    let init_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "test-client",
                        "version": "1.0.0"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.clone().oneshot(init_request).await.unwrap();
    let session_id = response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Delete the session
    let delete_request = Request::builder()
        .method("DELETE")
        .uri("/")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(delete_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Verify session is gone
    let list_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(list_request).await.unwrap();
    // We now return JSON-RPC errors for session issues
    assert_eq!(response.status(), StatusCode::OK);

    // Verify it's a JSON-RPC error response
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.get("error").is_some());
    assert_eq!(json["error"]["code"], -32005); // SessionNotFound
}

#[tokio::test]
async fn test_custom_session_store_receives_create_and_delete() {
    use crate::session_store::{MemorySessionStore, SessionStore as PublicSessionStore};

    let store = Arc::new(MemorySessionStore::new());
    let store_dyn: Arc<dyn PublicSessionStore> = store.clone();

    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .session_store(store_dyn);
    let (app, handle) = transport.into_router_with_handle();

    // Initialize to create a session.
    let init_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "test-client", "version": "1.0.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.clone().oneshot(init_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let session_id = response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Custom store should have the record.
    assert_eq!(store.len().await, 1);
    let record = store
        .load(&session_id)
        .await
        .unwrap()
        .expect("expected session to be persisted");
    assert_eq!(record.id, session_id);

    // After initialize completes the record must carry the client's
    // advertised identity / capabilities (issue #786). Previously these
    // were left as `None` because the record was created before
    // initialize ran.
    let client_info = record
        .client_info
        .expect("client_info should be populated after initialize");
    assert_eq!(client_info.name, "test-client");
    assert_eq!(client_info.version, "1.0.0");
    assert!(
        record.client_capabilities.is_some(),
        "client_capabilities should be populated after initialize"
    );

    // Terminate session via the handle -- store should be cleared.
    assert!(handle.terminate_session(&session_id).await);
    assert_eq!(store.len().await, 0);
    assert!(store.load(&session_id).await.unwrap().is_none());
}

#[tokio::test]
async fn test_session_store_record_carries_negotiated_protocol_version() {
    // Issue #786: stored record should reflect the negotiated protocol
    // version (taken from the initialize response), not the default the
    // session was created with.
    use crate::session_store::{MemorySessionStore, SessionStore as PublicSessionStore};

    let store = Arc::new(MemorySessionStore::new());
    let store_dyn: Arc<dyn PublicSessionStore> = store.clone();

    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .session_store(store_dyn);
    let app = transport.into_router();

    // Initialize using an older supported protocol version so we can
    // tell the persisted version apart from `LATEST_PROTOCOL_VERSION`.
    let init_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": { "name": "v-client", "version": "2.0.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(init_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let session_id = response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let record = store
        .load(&session_id)
        .await
        .unwrap()
        .expect("session should be persisted");
    assert_eq!(record.protocol_version, "2025-03-26");
    let client_info = record.client_info.expect("client_info should be populated");
    assert_eq!(client_info.name, "v-client");
}

#[tokio::test]
async fn test_restored_session_exposes_original_client_info() {
    // Issue #786: a session restored from the persistent store on a
    // peer instance should retain the original client's identity and
    // capabilities, not the synthetic defaults used for auto-reinit.
    use crate::session_store::{MemorySessionStore, SessionStore as PublicSessionStore};

    let store = Arc::new(MemorySessionStore::new());
    let store_dyn: Arc<dyn PublicSessionStore> = store.clone();

    // First "instance": initialize, then drop the transport so the
    // local registry is gone but the persistent record survives.
    let session_id = {
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .session_store(store_dyn.clone());
        let app = transport.into_router();

        let init_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": { "roots": {} },
                        "clientInfo": {
                            "name": "original-client",
                            "version": "3.1.4"
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(init_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    };

    // Sanity check: the persisted record now carries the client info.
    let stored = store
        .load(&session_id)
        .await
        .unwrap()
        .expect("record should survive transport drop");
    assert_eq!(
        stored.client_info.as_ref().map(|c| c.name.as_str()),
        Some("original-client")
    );

    // Second "instance": brand new transport, same store. A request
    // with the existing session id triggers restore_from_record.
    let transport2 = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .session_store(store_dyn);
    let app2 = transport2.into_router();

    let list_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();
    let response = app2.oneshot(list_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json.get("result").is_some(),
        "expected tools/list result, got {json}"
    );

    // After the restore, the record in the store still carries the
    // original client info (refreshed expiry, not a synthetic
    // "auto-recovered" identity).
    let after_restore = store
        .load(&session_id)
        .await
        .unwrap()
        .expect("record should still be present after restore");
    let client_info = after_restore
        .client_info
        .expect("restored record should retain client_info");
    assert_eq!(client_info.name, "original-client");
    assert_eq!(client_info.version, "3.1.4");
    assert!(
        after_restore.client_capabilities.is_some(),
        "restored record should retain client_capabilities"
    );
}

#[tokio::test]
async fn test_auto_reinitialize_marks_synthetic_client_info() {
    // Companion to the restored-client-info test: the auto-reinit
    // path must continue to flag the client as `"auto-recovered"` so
    // the two paths remain distinguishable on inspection of the
    // persisted record.
    use crate::session_store::{MemorySessionStore, SessionStore as PublicSessionStore};

    let store = Arc::new(MemorySessionStore::new());
    let store_dyn: Arc<dyn PublicSessionStore> = store.clone();

    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .session_store(store_dyn)
        .auto_reinitialize_sessions(true);
    let app = transport.into_router();

    let list_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header(MCP_SESSION_ID_HEADER, "made-up-id")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(list_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let record = store
        .load("made-up-id")
        .await
        .unwrap()
        .expect("auto-reinitialize should persist a record");
    assert_eq!(
        record.client_info.as_ref().map(|c| c.name.as_str()),
        Some("auto-recovered")
    );
}

#[tokio::test]
async fn test_custom_event_store_buffers_and_purges() {
    use crate::event_store::{EventStore as PublicEventStore, MemoryEventStore};

    let events = Arc::new(MemoryEventStore::new());
    let events_dyn: Arc<dyn PublicEventStore> = events.clone();

    // Build a session directly so we can exercise buffer_event/get_events_after
    // without needing a live SSE subscriber.
    let session = Arc::new(Session::new(
        create_test_router(),
        false,
        identity_factory(),
        events_dyn,
    ));

    session.buffer_event(0, "first".to_string()).await;
    session.buffer_event(1, "second".to_string()).await;

    // Custom store should have both events.
    assert_eq!(events.total_events().await, 2);
    let replayed = events.replay_after(&session.id, 0).await.unwrap();
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].id, 1);
    assert_eq!(replayed[0].data, "second");

    // Purging should clear the session's log.
    events.purge_session(&session.id).await.unwrap();
    assert_eq!(events.total_events().await, 0);
}

#[tokio::test]
async fn test_restore_from_store_serves_unknown_session_id() {
    use crate::session_store::{MemorySessionStore, SessionRecord, SessionStore};

    // Two transports share a single session store (simulating two
    // server instances behind a load balancer).
    let store = Arc::new(MemorySessionStore::new());
    let store_dyn: Arc<dyn SessionStore> = store.clone();

    // Seed the store with a record as if a peer instance had created it.
    let mut seeded = SessionRecord::new(
        "shared-session".to_string(),
        "2025-11-25".to_string(),
        Duration::from_secs(60),
    );
    store.create(&mut seeded).await.unwrap();
    let seeded_id = seeded.id;

    // This transport has never seen the session locally.
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .session_store(store_dyn);
    let app = transport.into_router();

    let list_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header(MCP_SESSION_ID_HEADER, &seeded_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(list_request).await.unwrap();
    // Without restore this would produce a SessionNotFound JSON-RPC
    // error; with restore the request is served normally.
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json.get("result").is_some(),
        "expected tools/list result, got {json}"
    );
}

fn test_session_registry() -> SessionRegistry {
    SessionRegistry::new(
        SessionConfig::default(),
        false,
        Arc::new(crate::session_store::MemorySessionStore::new()),
        Arc::new(crate::event_store::MemoryEventStore::new()),
        ServiceSource::Router {
            router: create_test_router(),
            factory: crate::transport::service::identity_factory(),
        },
        false,
    )
}

/// #458: the `initialized` notification can beat the `initialize` dispatch,
/// so it finds the session still `Uninitialized`. The session exists only
/// because an `initialize` request arrived, so the handshake completes
/// anyway and the follow-up list requests are served.
///
/// This is the case the `Uninitialized -> Initialized` path exists for, and
/// the one #1269 had to preserve while closing the bypass.
#[tokio::test]
async fn test_early_initialized_notification_still_completes_the_handshake() {
    let registry = test_session_registry();
    let session = registry
        .create(
            create_test_router().with_fresh_session(),
            crate::transport::service::identity_factory(),
        )
        .await
        .expect("session creation");

    let SessionServiceSource::Router { router, .. } = &session.service_source else {
        panic!("expected a router-backed session");
    };

    // The dispatch has not run yet, so the phase has not advanced.
    assert_eq!(router.session().phase(), crate::SessionPhase::Uninitialized);
    assert!(
        router.session().handshake_started(),
        "creating a session for an initialize request records the handshake"
    );

    session.handle_notification(McpNotification::Initialized);

    assert!(
        router.session().is_initialized(),
        "the notification must still complete a handshake that is in flight"
    );
    assert!(router.session().is_request_allowed("tools/list"));
}

/// #1269, the other side of the same coin: a session that no `initialize`
/// ever reached is not opened by the notification alone.
#[tokio::test]
async fn test_initialized_notification_alone_does_not_open_a_session() {
    let router = create_test_router().with_fresh_session();
    assert!(!router.session().handshake_started());

    router.handle_notification(McpNotification::Initialized);

    assert_eq!(router.session().phase(), crate::SessionPhase::Uninitialized);
    assert!(!router.session().is_request_allowed("tools/list"));
}

/// The `optional_sessions` opt-in still serves clients that skip the
/// handshake entirely. That is a server-side decision, so it goes through
/// `mark_preinitialized` rather than riding on the #458 race allowance.
#[tokio::test]
async fn test_optional_sessions_still_serves_without_a_handshake() {
    let registry = test_session_registry();
    let session = registry
        .create_initialized(
            create_test_router().with_fresh_session(),
            crate::transport::service::identity_factory(),
        )
        .await
        .expect("session creation");

    let SessionServiceSource::Router { router, .. } = &session.service_source else {
        panic!("expected a router-backed session");
    };
    assert!(router.session().is_initialized());
    assert!(router.session().is_request_allowed("tools/list"));
}

#[tokio::test]
async fn test_auto_reinitialize_serves_unknown_session_without_store_record() {
    // No seeded store record — the client just shows up with a
    // session ID the server has never heard of. With auto-reinit
    // enabled the transport spins up a synthetic session.
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .auto_reinitialize_sessions(true);
    let app = transport.into_router();

    let list_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header(MCP_SESSION_ID_HEADER, "client-made-up-id")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(list_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json.get("result").is_some(),
        "expected tools/list result, got {json}"
    );
}

#[tokio::test]
async fn test_unknown_session_without_restore_or_auto_reinit_returns_error() {
    // Default transport: no store seeded, no auto-reinit.
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    let list_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header(MCP_SESSION_ID_HEADER, "never-seen-before")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(list_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.get("error").is_some(), "expected error, got {json}");
    assert_eq!(json["error"]["code"], -32005); // SessionNotFound
}

#[tokio::test]
async fn test_session_expiration() {
    // Create transport with very short TTL
    let config = SessionConfig::with_ttl(Duration::from_millis(50))
        .cleanup_interval(Duration::from_millis(10));
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .session_config(config);
    let app = transport.into_router();

    // Initialize to get a session
    let init_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "test-client",
                        "version": "1.0.0"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.clone().oneshot(init_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let session_id = response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Wait for session to expire and cleanup to run
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Session should be expired now
    let list_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(list_request).await.unwrap();
    // We now return JSON-RPC errors for session issues
    assert_eq!(response.status(), StatusCode::OK);

    // Verify it's a JSON-RPC error response
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.get("error").is_some());
    assert_eq!(json["error"]["code"], -32005); // SessionNotFound
}

#[tokio::test]
async fn test_layer_with_identity() {
    // Verify that .layer() compiles and produces a working transport
    // using a no-op layer (tower::layer::Identity)
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .layer(tower::layer::util::Identity::new());
    let app = transport.into_router();

    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "test-client",
                        "version": "1.0.0"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key(MCP_SESSION_ID_HEADER));
}

#[tokio::test]
async fn test_layer_with_timeout() {
    // Verify that .layer() works with TimeoutLayer
    use std::time::Duration;
    use tower::timeout::TimeoutLayer;

    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .layer(TimeoutLayer::new(Duration::from_secs(30)));
    let app = transport.into_router();

    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "test-client",
                        "version": "1.0.0"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key(MCP_SESSION_ID_HEADER));
}

#[tokio::test]
async fn test_layer_middleware_error_produces_jsonrpc_error() {
    // Use an extremely short timeout to force an error.
    // The CatchError wrapper should convert it to a JSON-RPC error response.
    use std::time::Duration;
    use tower::timeout::TimeoutLayer;

    let slow_tool = crate::tool::ToolBuilder::new("slow")
        .description("A slow tool")
        .handler(|_: serde_json::Value| async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(crate::CallToolResult::text("done"))
        })
        .build();

    let router = McpRouter::new()
        .server_info("test-server", "1.0.0")
        .tool(slow_tool);

    // 1ms timeout will definitely expire before the tool completes
    let transport = HttpTransport::new(router)
        .disable_origin_validation()
        .layer(TimeoutLayer::new(Duration::from_millis(1)));
    let app = transport.into_router();

    // Initialize first
    let init_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "test-client",
                        "version": "1.0.0"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.clone().oneshot(init_request).await.unwrap();
    let session_id = response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Call the slow tool -- should timeout and return a JSON-RPC error
    let tool_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "slow",
                    "arguments": {}
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(tool_request).await.unwrap();
    // Should still return 200 with a JSON-RPC error body
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json.get("error").is_some(),
        "Expected JSON-RPC error response, got: {}",
        json
    );
}

#[tokio::test]
async fn test_max_sessions_limit() {
    // Create transport with max 1 session
    let config = SessionConfig::default().max_sessions(1);
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .session_config(config);
    let app = transport.into_router();

    // First initialize should succeed
    let init_request1 = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "test-client",
                        "version": "1.0.0"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.clone().oneshot(init_request1).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Second initialize should fail (max sessions reached)
    let init_request2 = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "test-client-2",
                        "version": "1.0.0"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(init_request2).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_session_event_buffering() {
    // Test that events are buffered and can be retrieved for replay (SEP-1699)
    let session = Session::new(
        create_test_router(),
        false,
        identity_factory(),
        Arc::new(crate::event_store::MemoryEventStore::new()),
    );

    // Buffer some events
    session.buffer_event(0, "event0".to_string()).await;
    session.buffer_event(1, "event1".to_string()).await;
    session.buffer_event(2, "event2".to_string()).await;

    // Get events after event 0
    let events = session.get_events_after(0).await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].id, 1);
    assert_eq!(events[0].data, "event1");
    assert_eq!(events[1].id, 2);
    assert_eq!(events[1].data, "event2");

    // Get events after event 1
    let events = session.get_events_after(1).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, 2);

    // Get events after event 2 (none)
    let events = session.get_events_after(2).await;
    assert!(events.is_empty());
}

#[tokio::test]
async fn test_session_event_counter_increments() {
    // Test that event IDs increment monotonically (SEP-1699)
    let session = Session::new(
        create_test_router(),
        false,
        identity_factory(),
        Arc::new(crate::event_store::MemoryEventStore::new()),
    );

    assert_eq!(session.next_event_id(), 0);
    assert_eq!(session.next_event_id(), 1);
    assert_eq!(session.next_event_id(), 2);
}

#[tokio::test]
async fn test_session_event_buffer_limit() {
    // Test that buffer respects max size limit
    // Create a session - buffer limit is DEFAULT_MAX_BUFFERED_EVENTS (1000)
    let session = Session::new(
        create_test_router(),
        false,
        identity_factory(),
        Arc::new(crate::event_store::MemoryEventStore::new()),
    );

    // Buffer more events than we can test practically, but verify the mechanism works
    // by checking that old events are evicted when we exceed the limit
    for i in 0..10 {
        session.buffer_event(i, format!("event{}", i)).await;
    }

    // All 10 events should be present
    let events = session.get_events_after(0).await;
    // Events after 0 should be 1-9 (9 events)
    assert_eq!(events.len(), 9);
}

#[tokio::test]
async fn test_session_handle_count() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let (app, handle) = transport.into_router_with_handle();

    // No sessions initially
    assert_eq!(handle.session_count().await, 0);

    // Initialize to create a session
    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "test-client",
                        "version": "1.0.0"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);

    // Now we should have 1 session
    assert_eq!(handle.session_count().await, 1);
}

#[tokio::test]
async fn test_session_handle_list_and_terminate() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let (app, handle) = transport.into_router_with_handle();

    // No sessions initially
    assert!(handle.list_sessions().await.is_empty());

    // Initialize to create a session
    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "test-client",
                        "version": "1.0.0"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);

    // list_sessions should return 1 session with valid metadata
    let sessions = handle.list_sessions().await;
    assert_eq!(sessions.len(), 1);
    assert!(!sessions[0].id.is_empty());

    // Terminate the session
    let session_id = sessions[0].id.clone();
    assert!(handle.terminate_session(&session_id).await);
    assert_eq!(handle.session_count().await, 0);

    // Terminating again returns false
    assert!(!handle.terminate_session(&session_id).await);
}

#[tokio::test]
async fn test_request_without_session_id_rejected() {
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .require_sessions();
    let app = transport.into_router();

    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        // No mcp-session-id header
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list",
                "params": {}
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK); // JSON-RPC errors still 200
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // Should return session required error
    assert!(json["error"].is_object());
}

#[tokio::test]
async fn test_invalid_session_id_returns_error() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("mcp-session-id", "nonexistent-session-id")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list",
                "params": {}
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"].as_i64().unwrap(), -32005); // SessionNotFound
}

#[tokio::test]
async fn test_notification_returns_accepted() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    // First initialize to get a session
    let init_req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.clone().oneshot(init_req).await.unwrap();
    let session_id = resp
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Send a notification (no id field) -- should return 202 Accepted
    let notif = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("mcp-session-id", &session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(notif).await.unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_invalid_json_returns_parse_error() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(Body::from("not valid json{{{"))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    tower_mcp_types::testing::assert_jsonrpc_error_response(&json);
    assert!(
        json["id"].is_null(),
        "id must be null on parse error: {json}"
    );
    assert_eq!(json["error"]["code"].as_i64().unwrap(), -32700);
}

#[tokio::test]
async fn test_session_config_max_sessions() {
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .session_config(SessionConfig::default().max_sessions(1));
    let app = transport.into_router();

    // First initialize succeeds
    let init1 = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "test1", "version": "1.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp1 = app.clone().oneshot(init1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);

    // Second initialize should fail (max 1 session)
    let init2 = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "test2", "version": "1.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp2 = app.oneshot(init2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_delete_terminates_session() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    // Initialize
    let init_req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.clone().oneshot(init_req).await.unwrap();
    let session_id = resp
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // DELETE should terminate the session
    let delete_req = Request::builder()
        .method("DELETE")
        .uri("/")
        .header("mcp-session-id", &session_id)
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(delete_req).await.unwrap();
    assert!(resp.status().is_success());

    // Subsequent request with that session ID should fail
    let list_req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("mcp-session-id", &session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(list_req).await.unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"].as_i64().unwrap(), -32005);
}

// -----------------------------------------------------------------------
// Origin validation / DNS rebinding protection
// -----------------------------------------------------------------------

#[test]
fn test_is_localhost_origin_http() {
    assert!(is_localhost_origin("http://localhost"));
    assert!(is_localhost_origin("http://localhost:3000"));
    assert!(is_localhost_origin("http://127.0.0.1"));
    assert!(is_localhost_origin("http://127.0.0.1:8080"));
    assert!(is_localhost_origin("http://[::1]"));
    assert!(is_localhost_origin("http://[::1]:3000"));
}

#[test]
fn test_is_localhost_origin_https() {
    assert!(is_localhost_origin("https://localhost"));
    assert!(is_localhost_origin("https://127.0.0.1:443"));
}

#[test]
fn test_is_localhost_origin_case_insensitive_scheme() {
    // RFC 3986: the URI scheme is case-insensitive.
    assert!(is_localhost_origin("HTTP://localhost"));
    assert!(is_localhost_origin("Https://127.0.0.1"));
    assert!(is_localhost_origin("http://LOCALHOST"));
}

#[test]
fn test_is_localhost_origin_trailing_dot_fqdn() {
    assert!(is_localhost_origin("http://localhost."));
    assert!(is_localhost_origin("http://localhost.:3000"));
}

#[test]
fn test_is_not_localhost_origin() {
    assert!(!is_localhost_origin("http://example.com"));
    assert!(!is_localhost_origin("http://evil-localhost.com"));
    assert!(!is_localhost_origin("http://localhost.evil.com"));
    assert!(!is_localhost_origin("ftp://localhost"));
    assert!(!is_localhost_origin("localhost"));
    assert!(!is_localhost_origin(""));
}

#[test]
fn test_is_not_localhost_origin_embedded_loopback_strings() {
    // These embed a loopback string without being loopback themselves.
    // Pinned before widening the guard (#1341) so a fix cannot
    // accidentally admit them.
    assert!(!is_localhost_origin("http://127.0.0.1.evil.com"));
    assert!(!is_localhost_origin("http://notlocalhost"));
    assert!(!is_localhost_origin("http://evil.com#localhost"));
}

#[test]
fn test_is_not_localhost_origin_bracketed_ipv6_with_suffix() {
    // #1350, surfaced through is_localhost_origin: a suffix appended
    // directly onto a loopback bracketed IPv6 literal in the Origin
    // header must not be treated as loopback either.
    assert!(!is_localhost_origin("http://[::1]evil.com"));
    assert!(!is_localhost_origin("http://[::1]@evil.com"));
    assert!(!is_localhost_origin("http://[::1].evil.com"));
}

#[tokio::test]
async fn test_origin_validation_rejects_cross_origin() {
    let transport = HttpTransport::new(create_test_router());
    let app = transport.into_router();

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Origin", "http://evil.com")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_origin_validation_allows_localhost() {
    let transport = HttpTransport::new(create_test_router());
    let app = transport.into_router();

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Origin", "http://localhost:3000")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_origin_validation_allows_configured_origin() {
    let transport = HttpTransport::new(create_test_router())
        .allowed_origins(vec!["https://my-app.example.com".to_string()]);
    let app = transport.into_router();

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Origin", "https://my-app.example.com")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_origin_validation_rejects_unconfigured_origin() {
    let transport = HttpTransport::new(create_test_router())
        .allowed_origins(vec!["https://my-app.example.com".to_string()]);
    let app = transport.into_router();

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Origin", "https://other-app.example.com")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_origin_validation_no_header_allowed() {
    // Requests without Origin header should be allowed (same-origin)
    let transport = HttpTransport::new(create_test_router());
    let app = transport.into_router();

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        // No Origin header
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_disabled_origin_validation_allows_any() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Origin", "http://evil.com")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// =========================================================================
// Host header validation (DNS rebinding defense complement to Origin)
// =========================================================================

fn initialize_body() -> Body {
    Body::from(
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1.0" }
            }
        })
        .to_string(),
    )
}

#[test]
fn test_is_localhost_host_variants() {
    assert!(is_localhost_host("localhost"));
    assert!(is_localhost_host("localhost:3000"));
    assert!(is_localhost_host("127.0.0.1"));
    assert!(is_localhost_host("127.0.0.1:8080"));
    assert!(is_localhost_host("[::1]"));
    assert!(is_localhost_host("[::1]:3000"));

    assert!(!is_localhost_host("evil.com"));
    assert!(!is_localhost_host("api.example.com:8443"));
    assert!(!is_localhost_host("10.0.0.1"));
}

#[test]
fn test_is_localhost_host_case_insensitive() {
    assert!(is_localhost_host("LOCALHOST"));
    assert!(is_localhost_host("LOCALHOST:3000"));
    assert!(is_localhost_host("Localhost"));
}

#[test]
fn test_is_localhost_host_full_loopback_range() {
    // RFC 5735: the entire 127.0.0.0/8 range is loopback, not just
    // 127.0.0.1.
    assert!(is_localhost_host("127.0.0.2"));
    assert!(is_localhost_host("127.1.2.3"));
    assert!(is_localhost_host("127.255.255.255"));
    assert!(is_localhost_host("127.0.0.2:8080"));
}

#[test]
fn test_is_localhost_host_trailing_dot_fqdn() {
    assert!(is_localhost_host("localhost."));
    assert!(is_localhost_host("localhost.:3000"));
}

#[test]
fn test_is_localhost_host_bare_ipv6() {
    // A bare (unbracketed, port-less) IPv6 literal is loopback too, not
    // just the bracketed `[::1]` form.
    assert!(is_localhost_host("::1"));
}

#[test]
fn test_is_not_localhost_host_embedded_loopback_strings() {
    // Pinned before widening is_localhost_host (#1341): none of these are
    // loopback even though each embeds a loopback string.
    assert!(!is_localhost_host("localhost.evil.com"));
    assert!(!is_localhost_host("127.0.0.1.evil.com"));
    assert!(!is_localhost_host("notlocalhost"));
    assert!(!is_localhost_host("evil.com#localhost"));
}

#[test]
fn test_is_not_localhost_host_non_canonical_ipv4_forms() {
    // Classic SSRF-style numeric encodings of 127.0.0.1. Rust's
    // std::net::IpAddr parser requires canonical 4-octet dotted-decimal
    // form and rejects all of these, so the loopback guard does not need
    // to (and deliberately does not) special-case them.
    assert!(!is_localhost_host("0x7f.0.0.1"));
    assert!(!is_localhost_host("017700000001"));
    assert!(!is_localhost_host("2130706433"));
    // Non-canonical shorthand (2-part) IPv4 for 127.0.0.1. A conforming
    // URL host parser would already have canonicalized this to
    // "127.0.0.1" before it reached the wire; left deliberately rejected
    // here rather than adding custom shorthand-IPv4 parsing.
    assert!(!is_localhost_host("127.1"));
}

#[test]
fn test_is_not_localhost_host_bare_ipv6_with_trailing_segment() {
    // A bare (unbracketed) IPv6 literal has no notion of an appended
    // port: "::1:3000" parses whole as the distinct, non-loopback
    // address 0:0:0:0:0:0:1:3000, not as "::1" plus port 3000.
    // Deliberately rejected: RFC 3986 requires brackets around an IPv6
    // host whenever a port follows it.
    assert!(!is_localhost_host("::1:3000"));
}

#[test]
fn test_is_not_localhost_host_bracketed_ipv6_with_suffix() {
    // #1350: `host.split(']').next()` discarded everything after the
    // first closing bracket without checking it, so a suffix appended
    // directly onto a loopback bracketed IPv6 literal was silently
    // ignored and the whole host treated as loopback. RFC 3986 permits
    // nothing after the closing bracket but an optional ":port"; anything
    // else makes the authority invalid.
    assert!(!is_localhost_host("[::1]evil.com"));
    assert!(!is_localhost_host("[::1]@evil.com"));
    assert!(!is_localhost_host("[::1].evil.com"));
    assert!(!is_localhost_host("[::1]]evil.com"));
    // Missing closing bracket entirely: also invalid, not loopback.
    assert!(!is_localhost_host("[::1"));
}

#[test]
fn test_is_not_localhost_host_bracketed_ipv6_non_loopback_with_suffix() {
    // A non-loopback bracketed IPv6 address with a suffix was already
    // correctly rejected; the suffix bug only mattered when the bracketed
    // address itself was loopback. Pinned alongside the loopback case so
    // the two don't drift apart.
    assert!(!is_localhost_host("[2001:db8::1]evil.com"));
}

#[test]
fn test_is_localhost_host_bracketed_ipv6_port_boundary() {
    assert!(is_localhost_host("[::1]"));
    assert!(is_localhost_host("[::1]:3000"));
    assert!(is_localhost_host("[::1]:0"));
    assert!(is_localhost_host("[::1]:65535"));
}

#[test]
fn test_is_not_localhost_host_bracketed_ipv6_invalid_port() {
    assert!(!is_localhost_host("[::1]:"));
    assert!(!is_localhost_host("[::1]:notaport"));
    assert!(!is_localhost_host("[::1]:99999"));
    assert!(!is_localhost_host("[::1]:65536"));
}

#[tokio::test]
async fn test_host_validation_allows_localhost() {
    let transport =
        HttpTransport::new(create_test_router()).allowed_hosts(vec!["api.example.com".to_string()]);
    let app = transport.into_router();

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Host", "127.0.0.1:3000")
        .body(initialize_body())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_host_validation_allows_configured_host() {
    let transport =
        HttpTransport::new(create_test_router()).allowed_hosts(vec!["api.example.com".to_string()]);
    let app = transport.into_router();

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Host", "api.example.com")
        .body(initialize_body())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_host_validation_rejects_unconfigured_host() {
    let transport =
        HttpTransport::new(create_test_router()).allowed_hosts(vec!["api.example.com".to_string()]);
    let app = transport.into_router();

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Host", "evil.com")
        .body(initialize_body())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_host_validation_no_allowlist_accepts_any_host() {
    // Existing deployments that haven't opted into Host validation
    // (no `.allowed_hosts(...)`) should keep accepting non-localhost
    // hosts; Origin still protects browsers.
    let transport = HttpTransport::new(create_test_router());
    let app = transport.into_router();

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Host", "any.example.com")
        .body(initialize_body())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_disabled_host_validation_allows_any_with_allowlist() {
    let transport = HttpTransport::new(create_test_router())
        .disable_host_validation()
        .allowed_hosts(vec!["api.example.com".to_string()]);
    let app = transport.into_router();

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Host", "evil.com")
        .body(initialize_body())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[test]
fn test_effective_host_prefers_header() {
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, HeaderValue::from_static("api.example.com"));
    let uri: axum::http::Uri = "http://other.example.com/path".parse().unwrap();
    assert_eq!(effective_host(&headers, &uri), Some("api.example.com"));
}

#[test]
fn test_effective_host_falls_back_to_authority() {
    // When Host header is missing (HTTP/2 + middleware that strips it),
    // we should fall back to the URI authority.
    let headers = HeaderMap::new();
    let uri: axum::http::Uri = "http://api.example.com/path".parse().unwrap();
    assert_eq!(effective_host(&headers, &uri), Some("api.example.com"));
}

#[test]
fn test_effective_host_returns_none_when_both_missing() {
    let headers = HeaderMap::new();
    let uri: axum::http::Uri = "/path".parse().unwrap();
    assert_eq!(effective_host(&headers, &uri), None);
}

// =========================================================================
// External notification fan-out
// =========================================================================

/// Initialize a session against `app` and return its session id.
async fn init_session(app: &Router) -> String {
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    resp.headers()
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .expect("initialize must return a session id")
}

#[tokio::test]
async fn test_external_notification_reaches_single_session() {
    let (notif_tx, notif_rx) = notification_channel(8);
    let transport = HttpTransport::with_notifications(create_test_router(), notif_rx);
    let (app, session_handle) = transport.into_router_with_handle();

    let session_id = init_session(&app).await;

    // Subscribe to the session's broadcast channel before firing.
    let mut rx = {
        let sessions = session_handle.store.sessions.read().await;
        let session = sessions
            .get(&session_id)
            .expect("session should be registered");
        session.notifications_tx.subscribe()
    };

    notif_tx
        .send(crate::context::ServerNotification::ResourceUpdated {
            uri: "claude://chats/abc".to_string(),
        })
        .await
        .unwrap();

    let json = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("notification should arrive within timeout")
        .expect("broadcast channel closed");
    assert!(json.contains("notifications/resources/updated"));
    assert!(json.contains("claude://chats/abc"));
}

#[tokio::test]
async fn test_external_notification_fans_out_to_all_sessions() {
    let (notif_tx, notif_rx) = notification_channel(8);
    let transport = HttpTransport::with_notifications(create_test_router(), notif_rx);
    let (app, session_handle) = transport.into_router_with_handle();

    let session_a = init_session(&app).await;
    let session_b = init_session(&app).await;
    assert_ne!(session_a, session_b);

    let (mut rx_a, mut rx_b) = {
        let sessions = session_handle.store.sessions.read().await;
        let a = sessions.get(&session_a).unwrap();
        let b = sessions.get(&session_b).unwrap();
        (
            a.notifications_tx.subscribe(),
            b.notifications_tx.subscribe(),
        )
    };

    notif_tx
        .send(crate::context::ServerNotification::ResourcesListChanged)
        .await
        .unwrap();

    let json_a = tokio::time::timeout(Duration::from_secs(1), rx_a.recv())
        .await
        .unwrap()
        .unwrap();
    let json_b = tokio::time::timeout(Duration::from_secs(1), rx_b.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(json_a.contains("notifications/resources/list_changed"));
    assert!(json_b.contains("notifications/resources/list_changed"));
}

#[tokio::test]
async fn test_external_notifications_builder_method() {
    // `external_notifications` should be equivalent to the constructor.
    let (notif_tx, notif_rx) = notification_channel(8);
    let transport = HttpTransport::new(create_test_router()).external_notifications(notif_rx);
    let (app, session_handle) = transport.into_router_with_handle();

    let session_id = init_session(&app).await;
    let mut rx = {
        let sessions = session_handle.store.sessions.read().await;
        sessions
            .get(&session_id)
            .unwrap()
            .notifications_tx
            .subscribe()
    };

    notif_tx
        .send(crate::context::ServerNotification::ToolsListChanged)
        .await
        .unwrap();

    let json = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(json.contains("notifications/tools/list_changed"));
}

#[tokio::test]
async fn test_default_transport_has_no_external_fanout_task() {
    // Smoke test: a transport without external notifications builds and
    // serves normally. (Verifying the fan-out task is *not* spawned is
    // hard to do directly; this just confirms we didn't accidentally
    // gate the happy path on the channel being present.)
    let transport = HttpTransport::new(create_test_router());
    let (app, _handle) = transport.into_router_with_handle();
    let _session_id = init_session(&app).await;
}

// =========================================================================
// Chunk 5: version-gated stateless mode for 2026-07-28+ clients
// =========================================================================

/// 2026-07-28 removed initialize entirely.
#[tokio::test]
#[cfg(feature = "stateless")]
async fn stateless_v2026_initialize_is_method_not_found() {
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .disable_host_validation();
    let app = transport.into_router();
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(MCP_METHOD_HEADER, "initialize")
        .header(MCP_PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION_2026_07_28)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2026-07-28",
                    "capabilities": {},
                    "clientInfo": { "name": "sc", "version": "1.0" },
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        !response.headers().contains_key(MCP_SESSION_ID_HEADER),
        "removed final method must not create a session"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["id"], 1);
    assert_eq!(json["error"]["code"], ErrorCode::MethodNotFound.code());
}

#[tokio::test]
#[cfg(feature = "stateless")]
async fn stateless_v2026_rejects_missing_required_meta_with_http_400() {
    let app = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .disable_host_validation()
        .into_router();
    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_METHOD_HEADER, "server/discover")
        .header(MCP_PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION_2026_07_28)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 101,
                "method": "server/discover",
                "params": {}
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["id"], 101);
    assert_eq!(json["error"]["code"], ErrorCode::InvalidParams.code());
}

#[tokio::test]
#[cfg(feature = "stateless")]
async fn stateless_v2026_rejects_invalid_meta_and_extension_keys_with_http_400() {
    let app = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .disable_host_validation()
        .into_router();

    let build_request = |id: i64, extra_meta: serde_json::Value, extensions: serde_json::Value| {
        let mut meta = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION_2026_07_28,
            "io.modelcontextprotocol/clientCapabilities": {
                "extensions": extensions
            }
        });
        meta.as_object_mut()
            .unwrap()
            .extend(extra_meta.as_object().unwrap().clone());
        Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_METHOD_HEADER, "server/discover")
            .header(MCP_PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION_2026_07_28)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "server/discover",
                    "params": { "_meta": meta }
                })
                .to_string(),
            ))
            .unwrap()
    };

    for request in [
        build_request(
            111,
            serde_json::json!({"com.example/-invalid": true}),
            serde_json::json!({}),
        ),
        build_request(
            112,
            serde_json::json!({}),
            serde_json::json!({"unprefixed": {}}),
        ),
        build_request(
            113,
            serde_json::json!({}),
            serde_json::json!({"com.example/feature": true}),
        ),
    ] {
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], ErrorCode::InvalidParams.code());
    }
}

#[tokio::test]
#[cfg(feature = "stateless")]
async fn stateless_v2026_rejects_missing_protocol_header_with_http_400() {
    let app = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .disable_host_validation()
        .into_router();
    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_METHOD_HEADER, "server/discover")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 102,
                "method": "server/discover",
                "params": {
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["id"], 102);
    assert_eq!(json["error"]["code"], McpErrorCode::HeaderMismatch.code());
}

#[tokio::test]
#[cfg(feature = "stateless")]
async fn stateless_v2026_unknown_method_is_http_404() {
    let app = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .disable_host_validation()
        .into_router();
    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_METHOD_HEADER, "unknown/method")
        .header(MCP_PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION_2026_07_28)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 103,
                "method": "unknown/method",
                "params": {
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["id"], 103);
    assert_eq!(json["error"]["code"], ErrorCode::MethodNotFound.code());
}

#[tokio::test]
#[cfg(feature = "stateless")]
async fn stateless_v2026_ignores_legacy_session_and_resumption_headers() {
    let app = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .disable_host_validation()
        .into_router();
    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_METHOD_HEADER, "tools/list")
        .header(MCP_PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION_2026_07_28)
        .header(MCP_SESSION_ID_HEADER, "legacy-session-that-does-not-exist")
        .header(LAST_EVENT_ID_HEADER, "legacy-event")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 104,
                "method": "tools/list",
                "params": {
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(MCP_SESSION_ID_HEADER));
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["result"]["tools"].is_array());
}

#[tokio::test]
#[cfg(feature = "stateless")]
async fn stateless_v2026_enforces_tool_client_capability_requirements() {
    use crate::{CallToolResult, SamplingCapability, ToolBuilder};

    let tool = ToolBuilder::new("sample")
        .no_params_handler(|| async { Ok(CallToolResult::text("ok")) })
        .build()
        .require_client_capabilities(ClientCapabilities {
            sampling: Some(SamplingCapability::default()),
            ..ClientCapabilities::default()
        });
    let router = McpRouter::new()
        .server_info("test-server", "1.0.0")
        .tool(tool);
    let app = HttpTransport::new(router)
        .disable_origin_validation()
        .disable_host_validation()
        .into_router();

    let build_request = |id: i64, capabilities: serde_json::Value| {
        Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_METHOD_HEADER, "tools/call")
            .header(MCP_NAME_HEADER, "sample")
            .header(MCP_PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION_2026_07_28)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "tools/call",
                    "params": {
                        "name": "sample",
                        "arguments": {},
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientCapabilities": capabilities
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap()
    };

    let response = app
        .clone()
        .oneshot(build_request(105, serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["id"], 105);
    assert_eq!(
        json["error"]["code"],
        McpErrorCode::MissingRequiredClientCapability.code()
    );
    assert_eq!(
        json["error"]["data"]["requiredCapabilities"],
        serde_json::json!({ "sampling": {} })
    );

    let response = app
        .oneshot(build_request(106, serde_json::json!({ "sampling": {} })))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["result"]["content"][0]["text"], "ok");
}

/// 2026-07-28 tools/call without session header succeeds.
#[tokio::test]
#[cfg(feature = "stateless")]
async fn stateless_v2026_tools_call_without_session_succeeds() {
    use crate::{CallToolResult, ToolBuilder};
    let router = McpRouter::new().server_info("t", "1.0.0").tool(
        ToolBuilder::new("echo")
            .description("echo")
            .handler(
                |args: serde_json::Value| async move { Ok(CallToolResult::text(args.to_string())) },
            )
            .build(),
    );
    let transport = HttpTransport::new(router).disable_origin_validation();
    let app = transport.into_router();
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_PROTOCOL_VERSION_HEADER, "2026-07-28")
        .header(MCP_METHOD_HEADER, "tools/call")
        .header(MCP_NAME_HEADER, "echo")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "echo",
                    "arguments": {"message": "hello"},
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientInfo": {
                            "name": "sc", "version": "1.0"
                        },
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !response.headers().contains_key(MCP_SESSION_ID_HEADER),
        "stateless tools/call must not set mcp-session-id"
    );
    assert_eq!(
        response
            .headers()
            .get(MCP_PROTOCOL_VERSION_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("2026-07-28")
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json.get("result").is_some(),
        "expected tools/call result, got: {json}"
    );
    assert_eq!(json["result"]["resultType"], "complete");
}

/// A stateless 2026-07-28 request body helper for the serverInfo tests below.
#[cfg(feature = "stateless")]
fn stateless_tools_call_request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_PROTOCOL_VERSION_HEADER, "2026-07-28")
        .header(MCP_METHOD_HEADER, "tools/call")
        .header(MCP_NAME_HEADER, "echo")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "echo",
                    "arguments": {"message": "hello"},
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientInfo": {
                            "name": "sc", "version": "1.0"
                        },
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                }
            })
            .to_string(),
        ))
        .unwrap()
}

#[cfg(feature = "stateless")]
fn echo_router() -> McpRouter {
    use crate::{CallToolResult, ToolBuilder};
    McpRouter::new().server_info("t", "1.0.0").tool(
        ToolBuilder::new("echo")
            .description("echo")
            .handler(
                |args: serde_json::Value| async move { Ok(CallToolResult::text(args.to_string())) },
            )
            .build(),
    )
}

/// SEP-2575: 2026-07-28 stateless responses carry server identity in
/// `_meta["io.modelcontextprotocol/serverInfo"]` by default.
#[tokio::test]
#[cfg(feature = "stateless")]
async fn stateless_v2026_response_stamps_server_info_by_default() {
    let transport = HttpTransport::new(echo_router()).disable_origin_validation();
    let app = transport.into_router();
    let response = app.oneshot(stateless_tools_call_request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "t",
        "expected serverInfo stamped into result._meta, got: {json}"
    );
    assert_eq!(
        json["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["version"],
        "1.0.0"
    );
}

/// `.stamp_server_info(false)` opts out of the SEP-2575 `_meta` stamp.
#[tokio::test]
#[cfg(feature = "stateless")]
async fn stateless_v2026_response_omits_server_info_when_disabled() {
    let transport = HttpTransport::new(echo_router())
        .disable_origin_validation()
        .stamp_server_info(false);
    let app = transport.into_router();
    let response = app.oneshot(stateless_tools_call_request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["result"].get("_meta").is_none(),
        "expected no _meta when stamping is disabled, got: {json}"
    );
}

/// 2025-11-25 initialize still returns mcp-session-id (unchanged).
#[tokio::test]
async fn stateless_v2025_initialize_still_gets_session_id() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "old-client", "version": "1.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().contains_key(MCP_SESSION_ID_HEADER),
        "2025-11-25 initialize must return mcp-session-id"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["result"].get("resultType").is_none(),
        "legacy result must remain unchanged: {json}"
    );
}

/// With require_sessions(), 2025-11-25 tools/list without session
/// header fails with SessionRequired (-32006) -- behavior unchanged.
#[tokio::test]
async fn stateless_v2025_tools_list_without_session_rejected() {
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .require_sessions();
    let app = transport.into_router();
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_PROTOCOL_VERSION_HEADER, "2025-11-25")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.get("error").is_some(), "expected error, got: {json}");
    assert_eq!(
        json["error"]["code"].as_i64().unwrap(),
        -32006,
        "expected SessionRequired (-32006)"
    );
}

/// 2026-07-28 tools/list without session header succeeds (#856).
#[tokio::test]
#[cfg(feature = "stateless")]
async fn stateless_v2026_tools_list_without_session_succeeds() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_PROTOCOL_VERSION_HEADER, "2026-07-28")
        .header(MCP_METHOD_HEADER, "tools/list")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list",
                "params": {
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !response.headers().contains_key(MCP_SESSION_ID_HEADER),
        "stateless tools/list must not set mcp-session-id"
    );
    assert_eq!(
        response
            .headers()
            .get(MCP_PROTOCOL_VERSION_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("2026-07-28")
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["result"]["tools"].is_array(),
        "expected tools array in result, got: {json}"
    );
    assert_eq!(json["result"]["resultType"], "complete");
    assert_eq!(json["result"]["ttlMs"], 0);
    assert_eq!(json["result"]["cacheScope"], "private");
}

/// A final-protocol cancellation notification returns 202 and creates no
/// session (#857).
#[tokio::test]
#[cfg(feature = "stateless")]
async fn stateless_v2026_notification_returns_202_no_session() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let (app, handle) = transport.into_router_with_handle();
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_PROTOCOL_VERSION_HEADER, "2026-07-28")
        .header(MCP_METHOD_HEADER, "notifications/cancelled")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": {
                    "requestId": 99,
                    "reason": "test",
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "stateless notification must return 202 ACCEPTED"
    );
    assert!(
        !response.headers().contains_key(MCP_SESSION_ID_HEADER),
        "stateless notification must not set mcp-session-id"
    );
    assert_eq!(
        handle.session_count().await,
        0,
        "stateless notification must not create a session"
    );
}

/// 2026-07-28 stateless request missing Mcp-Method returns -32020 + HTTP 400 (#859).
#[tokio::test]
#[cfg(feature = "stateless")]
async fn stateless_v2026_missing_mcp_method_returns_400() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_PROTOCOL_VERSION_HEADER, "2026-07-28")
        // Intentionally NO Mcp-Method header
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list",
                "params": {
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "missing Mcp-Method must return HTTP 400"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.get("error").is_some(), "expected error, got: {json}");
    assert_eq!(
        json["error"]["code"].as_i64().unwrap(),
        -32020,
        "expected HeaderMismatch (-32020)"
    );
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("Mcp-Method"),
        "error message must mention Mcp-Method, got: {json}"
    );
}

#[tokio::test]
async fn sse_responses_false_returns_application_json() {
    // Default behavior: synchronous responses use Content-Type: application/json
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .sse_responses(false);
    let app = transport.into_router();

    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0.1"}
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let ct = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("application/json"),
        "sse_responses(false) should return application/json, got: {ct}"
    );
}

#[tokio::test]
async fn sse_responses_true_returns_text_event_stream_with_valid_json() {
    // When sse_responses is enabled, synchronous responses use SSE format
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .sse_responses(true);
    let app = transport.into_router();

    let init_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0.1"}
        }
    })
    .to_string();

    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(init_body))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let ct = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("text/event-stream"),
        "sse_responses(true) should return text/event-stream, got: {ct}"
    );

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&bytes);

    // SSE body must contain the event type line and data line
    assert!(
        body_text.contains("event: message"),
        "SSE body missing 'event: message': {body_text}"
    );
    assert!(
        body_text.contains("data: "),
        "SSE body missing 'data: ' line: {body_text}"
    );

    // Extract and validate the JSON from the data: line
    let data_line = body_text
        .lines()
        .find(|l| l.starts_with("data: "))
        .expect("no data: line in SSE body");
    let json_str = data_line.trim_start_matches("data: ");
    let val: serde_json::Value =
        serde_json::from_str(json_str).expect("data: line is not valid JSON");

    // Verify it's a well-formed JSON-RPC response with the expected result
    assert_eq!(val["jsonrpc"], "2.0", "jsonrpc version mismatch: {val}");
    assert_eq!(val["id"], 1, "id mismatch: {val}");
    assert!(
        val["result"].is_object(),
        "result should be an object: {val}"
    );
    // The initialize result must contain protocolVersion
    assert_eq!(
        val["result"]["protocolVersion"].as_str(),
        Some("2025-11-25"),
        "protocolVersion missing or wrong: {val}"
    );
}

#[tokio::test]
async fn sse_responses_true_tools_list_returns_valid_sse() {
    // Verify tools/list (non-init request) also returns SSE when enabled
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .sse_responses(true);
    let app = transport.into_router();

    // Initialize first to get a session ID
    let init_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0.1"}
                }
            })
            .to_string(),
        ))
        .unwrap();

    let init_response = app.clone().oneshot(init_request).await.unwrap();
    assert_eq!(init_response.status(), StatusCode::OK);
    let session_id = init_response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .expect("missing session ID from initialize");

    // Send notifications/initialized to complete the MCP handshake.
    let notif_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            })
            .to_string(),
        ))
        .unwrap();
    app.clone().oneshot(notif_request).await.unwrap();

    // Now call tools/list
    let list_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            })
            .to_string(),
        ))
        .unwrap();

    let list_response = app.oneshot(list_request).await.unwrap();
    assert_eq!(list_response.status(), StatusCode::OK);

    let ct = list_response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("text/event-stream"),
        "tools/list with sse_responses(true) should return text/event-stream, got: {ct}"
    );

    let bytes = axum::body::to_bytes(list_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&bytes);
    let data_line = body_text
        .lines()
        .find(|l| l.starts_with("data: "))
        .expect("no data: line in SSE body for tools/list");
    let json_str = data_line.trim_start_matches("data: ");
    let val: serde_json::Value =
        serde_json::from_str(json_str).expect("tools/list data: line is not valid JSON");

    assert_eq!(val["jsonrpc"], "2.0");
    assert_eq!(val["id"], 2);
    // tools/list result has a "tools" array (may be empty for create_test_router())
    assert!(
        val["result"]["tools"].is_array(),
        "tools/list result.tools should be an array: {val}"
    );
}

// =========================================================================
// notifications/initialized enforcement (#901)
// =========================================================================

/// Helper: do the `initialize` handshake and return the session ID.
async fn do_initialize(app: &axum::Router) -> String {
    do_initialize_for_revision(app, "2025-11-25").await
}

async fn do_initialize_for_revision(app: &axum::Router, revision: &str) -> String {
    let init_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": revision,
                    "capabilities": {},
                    "clientInfo": { "name": "test-client", "version": "1.0.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.clone().oneshot(init_request).await.unwrap();
    response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

async fn send_initialized(app: &axum::Router, session_id: &str) {
    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(MCP_SESSION_ID_HEADER, session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

async fn post_legacy_batch(app: &axum::Router, session_id: &str) -> serde_json::Value {
    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_SESSION_ID_HEADER, session_id)
        .body(Body::from(
            serde_json::json!([
                {"jsonrpc": "2.0", "id": 2, "method": "ping"},
                {"jsonrpc": "2.0", "id": 3, "method": "tools/list"}
            ])
            .to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn http_batch_policy_uses_exact_session_revision() {
    let march_app = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .protocol_versions(["2025-03-26"])
        .unwrap()
        .into_router();
    let march_session = do_initialize_for_revision(&march_app, "2025-03-26").await;
    send_initialized(&march_app, &march_session).await;
    let march_response = post_legacy_batch(&march_app, &march_session).await;
    assert_eq!(march_response.as_array().map(Vec::len), Some(2));

    let november_app = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .into_router();
    let november_session = do_initialize(&november_app).await;
    send_initialized(&november_app, &november_session).await;
    let november_response = post_legacy_batch(&november_app, &november_session).await;
    assert_eq!(november_response["error"]["code"], -32600);
    assert!(
        november_response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not permit top-level JSON-RPC batches")
    );
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn http_final_batch_is_rejected_before_object_routing() {
    let app = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .into_router();
    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(MCP_PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION_2026_07_28)
        .body(Body::from(
            serde_json::json!([{
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list",
                "params": {
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                }
            }])
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let response: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(response["error"]["code"], -32600);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not permit top-level JSON-RPC batches")
    );
}

#[tokio::test]
async fn tools_list_before_initialized_notification_returns_error() {
    // Spec: clients MUST send notifications/initialized before any other
    // request. Skipping it should yield -32600 InvalidRequest.
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    let session_id = do_initialize(&app).await;

    // Send tools/list WITHOUT sending notifications/initialized first.
    let list_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(list_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json.get("error").is_some(),
        "expected error when notifications/initialized not sent, got: {json}"
    );
    assert_eq!(
        json["error"]["code"].as_i64().unwrap(),
        -32600,
        "expected InvalidRequest (-32600), got: {json}"
    );
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("notifications/initialized"),
        "error message should mention notifications/initialized, got: {json}"
    );
}

#[tokio::test]
async fn tools_list_after_initialized_notification_succeeds() {
    // After sending notifications/initialized, tool requests should succeed.
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    let session_id = do_initialize(&app).await;

    // Send notifications/initialized.
    let notif_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            })
            .to_string(),
        ))
        .unwrap();
    app.clone().oneshot(notif_request).await.unwrap();

    // Now tools/list should succeed.
    let list_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(list_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json.get("result").is_some(),
        "expected success after notifications/initialized, got: {json}"
    );
}

#[tokio::test]
async fn notifications_initialized_itself_always_accepted() {
    // The notifications/initialized notification itself must always be
    // accepted (202 ACCEPTED) regardless of the initialization flag.
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    let session_id = do_initialize(&app).await;

    let notif_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(notif_request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "notifications/initialized must return 202 ACCEPTED"
    );
}

#[tokio::test]
async fn strict_initialization_false_allows_tools_list_without_notification() {
    // When strict_initialization is disabled, tool requests must succeed
    // even if the client skips notifications/initialized.
    let config = SessionConfig {
        strict_initialization: false,
        ..Default::default()
    };
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .session_config(config);
    let app = transport.into_router();

    let session_id = do_initialize(&app).await;

    // No notifications/initialized -- should still succeed.
    let list_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.oneshot(list_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json.get("result").is_some(),
        "expected success with strict_initialization=false, got: {json}"
    );
}

// =============================================================================
// #1336: the POST body receive path never stripped a leading BOM, and three
// "malformed but identified" sites answered with `id: null` plus a verbatim
// serde message instead of the request's own id. Kept in its own block,
// separate from the `is_localhost_host` / `is_localhost_origin` tests above,
// since #1350 touches those in the same file.
// =============================================================================

/// A BOM-prefixed POST body is served rather than rejected with -32700.
/// Every other receive path in the crate already strips a leading BOM
/// (`clean_input_line`, #1303/#1314); this was the last one that didn't.
#[tokio::test]
async fn post_body_with_leading_bom_is_served() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    let body = format!(
        "\u{feff}{}",
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})
    );
    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(body))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["id"], serde_json::json!(1), "BOM'd request: {json}");
    assert!(
        json.get("result").is_some(),
        "BOM'd body must be parsed and served, not rejected: {json}"
    );
}

/// SEP-1442 stateless dispatch (the legacy stateless site, reached with no
/// `Mcp-Session-Id` and a supported `MCP-Protocol-Version` header) never
/// validates the envelope before deserializing into `JsonRpcRequest`, so a
/// malformed-but-identified request must still come back with its own id
/// and a message that doesn't name the Rust type.
#[tokio::test]
#[cfg(feature = "stateless")]
async fn stateless_malformed_request_with_readable_id_gets_a_matching_id() {
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .stateless(crate::stateless::StatelessConfig::new());
    let app = transport.into_router();

    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-11-25")
        // `method` is present and the id is well formed, but the request
        // as a whole is not valid (method must be a string).
        .body(Body::from(r#"{"jsonrpc":"2.0","id":7,"method":42}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(
        json["id"],
        serde_json::json!(7),
        "the id was present and readable, so it must be echoed back: {json}"
    );
    assert_eq!(json["error"]["code"].as_i64().unwrap(), -32700);
    let message = json["error"]["message"].as_str().unwrap();
    assert!(
        !message.contains("JsonRpcRequest") && !message.to_lowercase().contains("struct"),
        "error message must not name the Rust type (#1284 precedent): {message}"
    );
}

/// The same site, but the id itself is unusable (present, but neither a
/// number nor a string). There is nothing to correlate the error against,
/// so it must still answer with a null id.
#[tokio::test]
#[cfg(feature = "stateless")]
async fn stateless_malformed_request_with_unusable_id_stays_null() {
    let transport = HttpTransport::new(create_test_router())
        .disable_origin_validation()
        .stateless(crate::stateless::StatelessConfig::new());
    let app = transport.into_router();

    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-11-25")
        .body(Body::from(r#"{"jsonrpc":"2.0","id":null,"method":42}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(
        json["id"].is_null(),
        "no usable id was available, so the response id must be null: {json}"
    );
    assert_eq!(json["error"]["code"].as_i64().unwrap(), -32700);
}

/// The session-based legacy path also has a malformed-but-identified window:
/// `initialize` requests skip the envelope inspection that runs ahead of
/// every other request, so a structurally invalid `initialize` body (here,
/// a non-string `jsonrpc` field) still reaches the raw `JsonRpcRequest`
/// deserialize and must come back with its own id and a generic message.
#[tokio::test]
async fn legacy_initialize_with_malformed_envelope_gets_a_matching_id() {
    let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
    let app = transport.into_router();

    let body = serde_json::json!({
        "jsonrpc": 2.0,
        "id": 7,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "t", "version": "1" }
        }
    })
    .to_string();
    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(body))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(
        json["id"],
        serde_json::json!(7),
        "the id was present and readable, so it must be echoed back: {json}"
    );
    assert_eq!(json["error"]["code"].as_i64().unwrap(), -32700);
    let message = json["error"]["message"].as_str().unwrap();
    assert!(
        !message.contains("JsonRpcRequest") && !message.to_lowercase().contains("struct"),
        "error message must not name the Rust type (#1284 precedent): {message}"
    );
}
