//! OAuth 2.1 tower middleware for HTTP-level token validation.
//!
//! Provides [`OAuthLayer`] and [`OAuthService`] that implement bearer token
//! extraction, validation, and scope checking at the HTTP transport level.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tower::{Layer, ServiceExt};

use super::error::OAuthError;
use super::metadata::ProtectedResourceMetadata;
use super::scope::ScopePolicy;
use super::token::TokenValidator;

/// Tower layer that wraps services with OAuth 2.1 bearer token validation.
///
/// Applies [`OAuthService`] middleware that extracts and validates bearer
/// tokens from the `Authorization` header and requires the token audience to
/// contain the metadata resource identifier. On successful validation, injects
/// [`TokenClaims`](super::token::TokenClaims) into request extensions for downstream handlers.
///
/// # Example
///
/// ```rust,no_run
/// use tower_mcp::oauth::{OAuthLayer, JwtValidator, ProtectedResourceMetadata};
///
/// let validator = JwtValidator::from_secret(b"my-secret");
/// let metadata = ProtectedResourceMetadata::new("https://mcp.example.com")
///     .authorization_server("https://auth.example.com");
///
/// let layer = OAuthLayer::new(validator, metadata);
/// ```
#[derive(Clone)]
pub struct OAuthLayer<V: TokenValidator> {
    validator: V,
    metadata: ProtectedResourceMetadata,
    scope_policy: ScopePolicy,
    public_paths: Vec<String>,
}

impl<V: TokenValidator> OAuthLayer<V> {
    /// Create a new OAuth layer with the given token validator and metadata.
    pub fn new(validator: V, metadata: ProtectedResourceMetadata) -> Self {
        let metadata_path =
            ProtectedResourceMetadata::well_known_path_for_resource(&metadata.resource)
                .unwrap_or_else(|_| ProtectedResourceMetadata::well_known_path().to_string());
        Self {
            validator,
            metadata,
            scope_policy: ScopePolicy::new(),
            public_paths: vec![metadata_path],
        }
    }

    /// Set the default HTTP-level scope policy.
    ///
    /// Use [`ScopeEnforcementLayer`](super::ScopeEnforcementLayer), or the
    /// transport's `into_oauth_router` helper, for tool/resource/prompt scopes.
    pub fn scope_policy(mut self, policy: ScopePolicy) -> Self {
        self.scope_policy = policy;
        self
    }

    /// Add a path that does not require authentication.
    ///
    /// The resource's exact RFC 9728 metadata path is always public. Custom
    /// public paths are also matched exactly, not as prefixes.
    pub fn public_path(mut self, path: impl Into<String>) -> Self {
        self.public_paths.push(path.into());
        self
    }
}

impl<S, V: TokenValidator> Layer<S> for OAuthLayer<V> {
    type Service = OAuthService<S, V>;

    fn layer(&self, inner: S) -> Self::Service {
        OAuthService {
            inner,
            validator: self.validator.clone(),
            metadata: self.metadata.clone(),
            scope_policy: self.scope_policy.clone(),
            public_paths: self.public_paths.clone(),
        }
    }
}

/// Tower service that validates OAuth 2.1 bearer tokens on HTTP requests.
///
/// Created by [`OAuthLayer`]. For each incoming request:
///
/// 1. Checks if the request path is public (skips validation)
/// 2. Extracts the `Authorization: Bearer <token>` header
/// 3. Validates the token via [`TokenValidator`]
/// 4. Checks that the token audience contains the protected resource identifier
/// 5. Checks default scope requirements via [`ScopePolicy`]
/// 6. On success, injects [`TokenClaims`](super::token::TokenClaims) into request extensions
/// 7. On failure, returns the appropriate HTTP error with `WWW-Authenticate`
#[derive(Clone)]
pub struct OAuthService<S, V: TokenValidator> {
    inner: S,
    validator: V,
    metadata: ProtectedResourceMetadata,
    scope_policy: ScopePolicy,
    public_paths: Vec<String>,
}

impl<S, V> tower_service::Service<Request<Body>> for OAuthService<S, V>
where
    S: tower_service::Service<Request<Body>, Response = Response> + Clone + Send + 'static,
    S::Future: Send,
    S::Error: Into<crate::BoxError> + Send,
    V: TokenValidator,
{
    type Response = Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let path = req.uri().path().to_string();
        let public_paths = self.public_paths.clone();
        let validator = self.validator.clone();
        let metadata = self.metadata.clone();
        let scope_policy = self.scope_policy.clone();
        let inner = self.inner.clone();

        Box::pin(async move {
            // Skip validation for public paths
            if public_paths.contains(&path) {
                return inner.oneshot(req).await;
            }

            // Extract bearer token. The scheme is matched case-insensitively
            // per RFC 7235 via the shared `strip_scheme` helper; the
            // credential itself stays case-sensitive.
            let token = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| crate::auth::strip_scheme(s, "Bearer"))
                .map(|t| t.trim().to_string());

            let resource_metadata_url = metadata.well_known_url().ok();

            let Some(token) = token else {
                let error = OAuthError::MissingToken;
                return Ok(oauth_error_response(
                    &error,
                    resource_metadata_url.as_deref(),
                ));
            };

            // Validate token
            let claims = match validator.validate_token(&token).await {
                Ok(claims) => claims,
                Err(error) => {
                    return Ok(oauth_error_response(
                        &error,
                        resource_metadata_url.as_deref(),
                    ));
                }
            };

            // Enforce the canonical MCP resource audience independently of
            // the underlying JWT, introspection, or opaque-token validator.
            if !claims.audience_matches(&metadata.resource) {
                return Ok(oauth_error_response(
                    &OAuthError::InvalidAudience,
                    resource_metadata_url.as_deref(),
                ));
            }

            // Check default scope requirements
            if let Err(error) = scope_policy.check_default(&claims) {
                return Ok(oauth_error_response(
                    &error,
                    resource_metadata_url.as_deref(),
                ));
            }

            // Inject claims into request extensions
            let mut req = req;
            req.extensions_mut().insert(claims);
            inner.oneshot(req).await
        })
    }
}

/// Build an HTTP error response for an OAuth error.
///
/// Returns the appropriate status code (401 or 403) with the
/// `WWW-Authenticate` header and a JSON-RPC error body.
fn oauth_error_response(error: &OAuthError, resource_metadata_url: Option<&str>) -> Response {
    let status = match error.status_code() {
        401 => StatusCode::UNAUTHORIZED,
        403 => StatusCode::FORBIDDEN,
        _ => StatusCode::UNAUTHORIZED,
    };

    let www_authenticate = error.www_authenticate(resource_metadata_url);

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "error": {
            "code": -32001,
            "message": error.to_string()
        },
        "id": null
    });

    let mut response = (status, axum::Json(body)).into_response();
    response.headers_mut().insert(
        "WWW-Authenticate",
        www_authenticate
            .parse()
            .unwrap_or_else(|_| "Bearer".parse().unwrap()),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tower::ServiceExt;
    use tower_service::Service;

    /// A minimal inner service that returns 200 OK for any request
    #[derive(Clone)]
    struct OkService;

    impl tower_service::Service<Request<Body>> for OkService {
        type Response = Response;
        type Error = std::convert::Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: Request<Body>) -> Self::Future {
            Box::pin(async {
                Ok(Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::empty())
                    .unwrap())
            })
        }
    }

    fn test_validator() -> crate::oauth::JwtValidator {
        crate::oauth::JwtValidator::from_secret(b"test-secret").disable_exp_validation()
    }

    fn test_metadata() -> ProtectedResourceMetadata {
        ProtectedResourceMetadata::new("https://mcp.example.com")
            .authorization_server("https://auth.example.com")
    }

    fn make_token(claims: &serde_json::Value) -> String {
        let mut claims = claims.clone();
        claims
            .as_object_mut()
            .unwrap()
            .entry("aud")
            .or_insert_with(|| serde_json::json!("https://mcp.example.com"));
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(b"test-secret"),
        )
        .unwrap()
    }

    fn make_token_without_default_audience(claims: &serde_json::Value) -> String {
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            claims,
            &jsonwebtoken::EncodingKey::from_secret(b"test-secret"),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn test_missing_token_returns_401() {
        let layer = OAuthLayer::new(test_validator(), test_metadata());
        let mut service = layer.layer(OkService);

        let req = Request::builder().uri("/mcp").body(Body::empty()).unwrap();

        let resp = service.ready().await.unwrap().call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(resp.headers().contains_key("WWW-Authenticate"));
    }

    #[tokio::test]
    async fn test_bearer_scheme_is_case_insensitive() {
        let layer = OAuthLayer::new(test_validator(), test_metadata());
        let mut service = layer.layer(OkService);

        let token = make_token(&serde_json::json!({"sub": "user123"}));

        for scheme in ["Bearer", "bearer", "BEARER", "BeArEr"] {
            let req = Request::builder()
                .uri("/mcp")
                .header("Authorization", format!("{scheme} {token}"))
                .body(Body::empty())
                .unwrap();

            let resp = service.ready().await.unwrap().call(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "scheme: {scheme}");
        }
    }

    #[tokio::test]
    async fn test_bearer_like_scheme_is_rejected() {
        let layer = OAuthLayer::new(test_validator(), test_metadata());
        let mut service = layer.layer(OkService);

        let token = make_token(&serde_json::json!({"sub": "user123"}));

        // `Bearerish` must not match `Bearer`: the scheme is case-insensitive
        // but it must still be exactly `Bearer`, followed by whitespace.
        let req = Request::builder()
            .uri("/mcp")
            .header("Authorization", format!("Bearerish {token}"))
            .body(Body::empty())
            .unwrap();

        let resp = service.ready().await.unwrap().call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_bearer_credential_stays_case_sensitive() {
        let layer = OAuthLayer::new(test_validator(), test_metadata());
        let mut service = layer.layer(OkService);

        let token = make_token(&serde_json::json!({"sub": "user123"}));
        let uppercased_token = token.to_uppercase();
        assert_ne!(token, uppercased_token, "test token must contain letters");

        let req = Request::builder()
            .uri("/mcp")
            .header("Authorization", format!("bearer {uppercased_token}"))
            .body(Body::empty())
            .unwrap();

        let resp = service.ready().await.unwrap().call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_valid_token_passes() {
        let layer = OAuthLayer::new(test_validator(), test_metadata());
        let mut service = layer.layer(OkService);

        let token = make_token(&serde_json::json!({"sub": "user123"}));

        let req = Request::builder()
            .uri("/mcp")
            .header("Authorization", format!("Bearer {}", token))
            .body(Body::empty())
            .unwrap();

        let resp = service.ready().await.unwrap().call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_invalid_token_returns_401() {
        let layer = OAuthLayer::new(test_validator(), test_metadata());
        let mut service = layer.layer(OkService);

        let req = Request::builder()
            .uri("/mcp")
            .header("Authorization", "Bearer not-a-valid-jwt")
            .body(Body::empty())
            .unwrap();

        let resp = service.ready().await.unwrap().call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(resp.headers().contains_key("WWW-Authenticate"));
    }

    #[tokio::test]
    async fn test_well_known_path_is_public() {
        let layer = OAuthLayer::new(test_validator(), test_metadata());
        let mut service = layer.layer(OkService);

        let req = Request::builder()
            .uri("/.well-known/oauth-protected-resource")
            .body(Body::empty())
            .unwrap();

        let resp = service.ready().await.unwrap().call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_custom_public_path() {
        let layer = OAuthLayer::new(test_validator(), test_metadata()).public_path("/health");
        let mut service = layer.layer(OkService);

        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();

        let resp = service.ready().await.unwrap().call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_public_paths_are_exact() {
        let layer = OAuthLayer::new(test_validator(), test_metadata()).public_path("/health");
        let mut service = layer.layer(OkService);

        for path in ["/health/private", "/.well-known/not-oauth"] {
            let req = Request::builder().uri(path).body(Body::empty()).unwrap();
            let resp = service.ready().await.unwrap().call(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "path: {path}");
        }
    }

    #[tokio::test]
    async fn test_missing_or_wrong_audience_is_rejected() {
        let layer = OAuthLayer::new(test_validator(), test_metadata());
        let mut service = layer.layer(OkService);

        for claims in [
            serde_json::json!({"sub": "user"}),
            serde_json::json!({"sub": "user", "aud": "https://other.example.com"}),
        ] {
            let token = make_token_without_default_audience(&claims);
            let req = Request::builder()
                .uri("/mcp")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            let resp = service.ready().await.unwrap().call(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn test_path_resource_uses_path_aware_metadata_url_and_public_route() {
        let metadata = ProtectedResourceMetadata::new("https://mcp.example.com/tenant/mcp")
            .authorization_server("https://auth.example.com");
        let layer = OAuthLayer::new(test_validator(), metadata);
        let mut service = layer.layer(OkService);

        let public = Request::builder()
            .uri("/.well-known/oauth-protected-resource/tenant/mcp")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            service
                .ready()
                .await
                .unwrap()
                .call(public)
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );

        let protected = Request::builder()
            .uri("/tenant/mcp")
            .body(Body::empty())
            .unwrap();
        let response = service
            .ready()
            .await
            .unwrap()
            .call(protected)
            .await
            .unwrap();
        let challenge = response
            .headers()
            .get("WWW-Authenticate")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            challenge.contains(
                "https://mcp.example.com/.well-known/oauth-protected-resource/tenant/mcp"
            )
        );
    }

    #[tokio::test]
    async fn test_insufficient_scope_returns_403() {
        let policy = ScopePolicy::new().default_scope("mcp:admin");
        let layer = OAuthLayer::new(test_validator(), test_metadata()).scope_policy(policy);
        let mut service = layer.layer(OkService);

        let token = make_token(&serde_json::json!({"sub": "user", "scope": "mcp:read"}));

        let req = Request::builder()
            .uri("/mcp")
            .header("Authorization", format!("Bearer {}", token))
            .body(Body::empty())
            .unwrap();

        let resp = service.ready().await.unwrap().call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let www_auth = resp
            .headers()
            .get("WWW-Authenticate")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(www_auth.contains("insufficient_scope"));
    }

    #[tokio::test]
    async fn test_sufficient_scope_passes() {
        let policy = ScopePolicy::new().default_scope("mcp:read");
        let layer = OAuthLayer::new(test_validator(), test_metadata()).scope_policy(policy);
        let mut service = layer.layer(OkService);

        let token = make_token(&serde_json::json!({"sub": "user", "scope": "mcp:read mcp:write"}));

        let req = Request::builder()
            .uri("/mcp")
            .header("Authorization", format!("Bearer {}", token))
            .body(Body::empty())
            .unwrap();

        let resp = service.ready().await.unwrap().call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_www_authenticate_includes_metadata_url() {
        let layer = OAuthLayer::new(test_validator(), test_metadata());
        let mut service = layer.layer(OkService);

        let req = Request::builder().uri("/mcp").body(Body::empty()).unwrap();

        let resp = service.ready().await.unwrap().call(req).await.unwrap();
        let www_auth = resp
            .headers()
            .get("WWW-Authenticate")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(www_auth.contains("resource_metadata="));
        assert!(www_auth.contains("mcp.example.com"));
    }
}
