//! Integration tests for JWKS-based JWT validation.
//!
//! These tests spin up a lightweight axum server serving a JWKS endpoint,
//! then use `JwksValidator` to validate tokens signed with keys from that
//! endpoint.

#![cfg(feature = "jwks")]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::json;
use tokio::sync::RwLock;
use tower_mcp::oauth::{JwksValidator, TokenValidator};

/// RSA key pair generated for testing (2048-bit).
///
/// These are test-only keys and are NOT used in production.
fn test_rsa_keypair() -> (EncodingKey, serde_json::Value) {
    // Use a known RSA private key for testing
    let rsa_private_pem = include_str!("fixtures/rsa_private.pem");
    let rsa_public_jwk = include_str!("fixtures/rsa_public.jwk.json");

    let encoding_key = EncodingKey::from_rsa_pem(rsa_private_pem.as_bytes()).unwrap();
    let public_jwk: serde_json::Value = serde_json::from_str(rsa_public_jwk).unwrap();

    (encoding_key, public_jwk)
}

/// Spin up a mock JWKS server returning the given JWK set JSON.
async fn start_jwks_server(
    jwks_json: Arc<RwLock<serde_json::Value>>,
) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/.well-known/jwks.json",
        get(move || {
            let jwks = jwks_json.clone();
            async move {
                let value = jwks.read().await;
                axum::Json(value.clone())
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (url, handle)
}

/// Create a signed JWT with the given claims and encoding key.
fn create_signed_token(
    claims: &serde_json::Value,
    encoding_key: &EncodingKey,
    kid: Option<&str>,
) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = kid.map(String::from);

    jsonwebtoken::encode(&header, claims, encoding_key).unwrap()
}

#[tokio::test]
async fn test_jwks_validator_validates_rsa_token() {
    let (encoding_key, public_jwk) = test_rsa_keypair();

    let jwks_json = Arc::new(RwLock::new(json!({
        "keys": [public_jwk]
    })));

    let (base_url, _handle) = start_jwks_server(jwks_json).await;

    let validator = JwksValidator::builder(format!("{}/.well-known/jwks.json", base_url))
        .disable_exp_validation()
        .build()
        .await
        .expect("failed to build JwksValidator");

    let claims = json!({
        "sub": "user123",
        "scope": "mcp:read mcp:write"
    });

    let token = create_signed_token(&claims, &encoding_key, Some("test-key-1"));

    let result = validator.validate_token(&token).await;
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result.err());

    let validated = result.unwrap();
    assert_eq!(validated.sub.as_deref(), Some("user123"));
    assert!(validated.has_scope("mcp:read"));
    assert!(validated.has_scope("mcp:write"));
}

#[tokio::test]
async fn test_jwks_validator_refreshes_on_kid_miss() {
    let (encoding_key, mut public_jwk) = test_rsa_keypair();

    // Start with kid = "old-key"
    public_jwk["kid"] = json!("old-key");
    let jwks_json = Arc::new(RwLock::new(json!({
        "keys": [public_jwk.clone()]
    })));

    let (base_url, _handle) = start_jwks_server(jwks_json.clone()).await;

    let _initial_validator = JwksValidator::builder(format!("{}/.well-known/jwks.json", base_url))
        .disable_exp_validation()
        .build()
        .await
        .unwrap();

    // Sign a token with kid = "new-key" (not in the initial JWKS)
    let claims = json!({"sub": "user456"});
    let token = create_signed_token(&claims, &encoding_key, Some("new-key"));

    // Update the JWKS endpoint with the new kid (simulating key rotation)
    {
        let mut jwk_updated = public_jwk.clone();
        jwk_updated["kid"] = json!("new-key");
        let mut jwks = jwks_json.write().await;
        *jwks = json!({ "keys": [jwk_updated] });
    }

    // Build a new validator that will fetch the updated JWKS
    let validator = JwksValidator::builder(format!("{}/.well-known/jwks.json", base_url))
        .disable_exp_validation()
        .build()
        .await
        .unwrap();

    let result = validator.validate_token(&token).await;
    assert!(
        result.is_ok(),
        "Expected Ok after JWKS update, got: {:?}",
        result.err()
    );
    assert_eq!(result.unwrap().sub.as_deref(), Some("user456"));
}

#[tokio::test]
async fn test_jwks_validator_stale_fallback() {
    let (encoding_key, public_jwk) = test_rsa_keypair();

    let jwks_json = Arc::new(RwLock::new(json!({
        "keys": [public_jwk]
    })));

    let (base_url, handle) = start_jwks_server(jwks_json).await;

    // Use a very short TTL so cache expires quickly
    let validator = JwksValidator::builder(format!("{}/.well-known/jwks.json", base_url))
        .default_ttl(Duration::from_millis(50))
        .disable_exp_validation()
        .build()
        .await
        .unwrap();

    // Token should validate initially
    let claims = json!({"sub": "stale-test"});
    let token = create_signed_token(&claims, &encoding_key, Some("test-key-1"));

    let result = validator.validate_token(&token).await;
    assert!(result.is_ok());

    // Abort the JWKS server so refresh will fail
    handle.abort();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Stale cache should still validate the token (refresh fails but stale keys used)
    // Note: min_refresh_interval (10s) will prevent immediate re-fetch, so stale keys are used
    let result = validator.validate_token(&token).await;
    assert!(
        result.is_ok(),
        "Expected stale cache to validate, got: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn test_jwks_validator_initial_fetch_failure() {
    // Point to a non-existent server
    let result = JwksValidator::builder("http://127.0.0.1:1/.well-known/jwks.json")
        .build()
        .await;

    assert!(result.is_err(), "Expected build() to fail");
}

#[tokio::test]
async fn test_jwks_validator_with_audience_and_issuer() {
    let (encoding_key, public_jwk) = test_rsa_keypair();

    let jwks_json = Arc::new(RwLock::new(json!({
        "keys": [public_jwk]
    })));

    let (base_url, _handle) = start_jwks_server(jwks_json).await;

    let validator = JwksValidator::builder(format!("{}/.well-known/jwks.json", base_url))
        .expected_audience("https://mcp.example.com")
        .expected_issuer("https://auth.example.com")
        .disable_exp_validation()
        .build()
        .await
        .unwrap();

    // Token with correct audience and issuer
    let claims = json!({
        "sub": "user789",
        "aud": "https://mcp.example.com",
        "iss": "https://auth.example.com",
    });
    let token = create_signed_token(&claims, &encoding_key, Some("test-key-1"));
    let result = validator.validate_token(&token).await;
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result.err());

    // Token with wrong audience
    let bad_claims = json!({
        "sub": "user789",
        "aud": "https://wrong.example.com",
        "iss": "https://auth.example.com",
    });
    let bad_token = create_signed_token(&bad_claims, &encoding_key, Some("test-key-1"));
    let result = validator.validate_token(&bad_token).await;
    assert!(result.is_err());
}
