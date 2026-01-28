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
use tower::Layer;

use super::error::OAuthError;
use super::metadata::ProtectedResourceMetadata;
use super::scope::ScopePolicy;
use super::token::TokenValidator;

/// Tower layer that wraps services with OAuth 2.1 bearer token validation.
///
/// Applies [`OAuthService`] middleware that extracts and validates bearer
/// tokens from the `Authorization` header. On successful validation, injects
/// [`TokenClaims`] into request extensions for downstream handlers.
///
/// # Example
///
/// ```rust,no_run
/// use tower_mcp::oauth::{OAuthLayer, JwtValidator, ProtectedResourceMetadata, ScopePolicy};
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
        Self {
            validator,
            metadata,
            scope_policy: ScopePolicy::new(),
            public_paths: vec![ProtectedResourceMetadata::well_known_path().to_string()],
        }
    }

    /// Set the scope policy for per-operation scope enforcement.
    pub fn scope_policy(mut self, policy: ScopePolicy) -> Self {
        self.scope_policy = policy;
        self
    }

    /// Add a path that does not require authentication.
    ///
    /// The `/.well-known/oauth-protected-resource` path is always public.
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
/// 4. Checks default scope requirements via [`ScopePolicy`]
/// 5. On success, injects [`TokenClaims`] into request extensions
/// 6. On failure, returns the appropriate HTTP error with `WWW-Authenticate`
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
        let mut inner = self.inner.clone();

        Box::pin(async move {
            // Skip validation for public paths
            if public_paths.iter().any(|p| path.starts_with(p.as_str())) {
                return inner.call(req).await;
            }

            // Also skip well-known paths that may be nested under a mount point
            if path.contains("/.well-known/") {
                return inner.call(req).await;
            }

            // Extract bearer token
            let token = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer "))
                .map(|t| t.trim().to_string());

            let resource_metadata_url =
                format!("{}/.well-known/oauth-protected-resource", metadata.resource);

            let Some(token) = token else {
                let error = OAuthError::MissingToken;
                return Ok(oauth_error_response(&error, Some(&resource_metadata_url)));
            };

            // Validate token
            let claims = match validator.validate_token(&token).await {
                Ok(claims) => claims,
                Err(error) => {
                    return Ok(oauth_error_response(&error, Some(&resource_metadata_url)));
                }
            };

            // Check default scope requirements
            if let Err(error) = scope_policy.check_default(&claims) {
                return Ok(oauth_error_response(&error, Some(&resource_metadata_url)));
            }

            // Inject claims into request extensions
            let mut req = req;
            req.extensions_mut().insert(claims);
            inner.call(req).await
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
