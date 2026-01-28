//! Authentication middleware helpers for MCP servers
//!
//! This module provides helper types and layers for common authentication patterns.
//! Since tower-mcp is built on Tower, standard tower middleware can be used directly.
//!
//! # Patterns
//!
//! ## API Key Authentication
//!
//! ```rust,ignore
//! // Requires the `http` feature
//! use tower_mcp::auth::{AuthConfig, ApiKeyValidator};
//! use tower_mcp::{McpRouter, HttpTransport};
//! use std::sync::Arc;
//!
//! // Simple in-memory API key validator
//! let valid_keys = vec!["sk-test-key-123".to_string()];
//! let validator = ApiKeyValidator::new(valid_keys);
//!
//! let router = McpRouter::new().server_info("my-server", "1.0.0");
//! let transport = HttpTransport::new(router);
//!
//! // The auth layer extracts the key from the Authorization header
//! // and validates it using the provided validator
//! ```
//!
//! ## Bearer Token Authentication
//!
//! For OAuth2/JWT tokens, use the `BearerTokenValidator` trait to implement
//! custom validation logic (e.g., JWT verification, token introspection).
//!
//! ## Custom Authentication
//!
//! You can implement custom auth by creating a Tower layer. See the examples
//! directory for a complete example.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::Layer;
use tower_service::Service;

/// Result of an authentication attempt
#[derive(Debug, Clone)]
pub enum AuthResult {
    /// Authentication succeeded with optional user/client info
    Authenticated(Option<AuthInfo>),
    /// Authentication failed with a reason
    Failed(AuthError),
}

/// Information about an authenticated client
#[derive(Debug, Clone)]
pub struct AuthInfo {
    /// Client/user identifier
    pub client_id: String,
    /// Optional additional claims or metadata
    pub claims: Option<serde_json::Value>,
}

/// Authentication error
#[derive(Debug, Clone)]
pub struct AuthError {
    /// Error code (e.g., "invalid_token", "expired_token")
    pub code: String,
    /// Human-readable error message
    pub message: String,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AuthError {}

// =============================================================================
// Validation Trait
// =============================================================================

/// Trait for validating authentication credentials.
///
/// Implement this trait to provide custom authentication logic for use
/// with [`AuthLayer`] and [`AuthService`].
///
/// The credential string passed to [`validate`](Validate::validate) is the
/// value extracted from the configured request header after parsing
/// (e.g., the token portion of `"Bearer sk-123"`).
///
/// # Example
///
/// ```rust
/// use tower_mcp::auth::{Validate, AuthResult, AuthInfo, AuthError};
///
/// #[derive(Clone)]
/// struct MyValidator;
///
/// impl Validate for MyValidator {
///     async fn validate(&self, credential: &str) -> AuthResult {
///         if credential.starts_with("sk-") {
///             AuthResult::Authenticated(Some(AuthInfo {
///                 client_id: credential.to_string(),
///                 claims: None,
///             }))
///         } else {
///             AuthResult::Failed(AuthError {
///                 code: "invalid_credential".to_string(),
///                 message: "Credential must start with sk-".to_string(),
///             })
///         }
///     }
/// }
/// ```
pub trait Validate: Clone + Send + Sync + 'static {
    /// Validate a credential and return the authentication result.
    fn validate(&self, credential: &str) -> impl Future<Output = AuthResult> + Send;
}

// =============================================================================
// API Key Authentication
// =============================================================================

/// Simple in-memory API key validator
///
/// For production use, consider:
/// - Database-backed validation
/// - Caching with TTL
/// - Rate limiting per key
#[derive(Debug, Clone)]
pub struct ApiKeyValidator {
    valid_keys: Arc<HashSet<String>>,
}

impl ApiKeyValidator {
    /// Create a new validator with a list of valid API keys
    pub fn new(keys: impl IntoIterator<Item = String>) -> Self {
        Self {
            valid_keys: Arc::new(keys.into_iter().collect()),
        }
    }

    /// Add a key to the valid set
    pub fn add_key(&mut self, key: String) {
        Arc::make_mut(&mut self.valid_keys).insert(key);
    }

    /// Check if a key is valid
    pub fn is_valid(&self, key: &str) -> bool {
        self.valid_keys.contains(key)
    }
}

impl Validate for ApiKeyValidator {
    async fn validate(&self, key: &str) -> AuthResult {
        if self.valid_keys.contains(key) {
            AuthResult::Authenticated(Some(AuthInfo {
                client_id: format!("api_key:{}", &key[..8.min(key.len())]),
                claims: None,
            }))
        } else {
            AuthResult::Failed(AuthError {
                code: "invalid_api_key".to_string(),
                message: "The provided API key is not valid".to_string(),
            })
        }
    }
}

// =============================================================================
// Bearer Token Authentication
// =============================================================================

/// Simple bearer token validator that checks against a static set of tokens.
///
/// For production, implement [`Validate`] with:
/// - JWT verification using a signing key
/// - OAuth2 token introspection
/// - OIDC ID token validation
#[derive(Debug, Clone)]
pub struct StaticBearerValidator {
    valid_tokens: Arc<HashSet<String>>,
}

impl StaticBearerValidator {
    /// Create a new validator with a list of valid tokens
    pub fn new(tokens: impl IntoIterator<Item = String>) -> Self {
        Self {
            valid_tokens: Arc::new(tokens.into_iter().collect()),
        }
    }
}

impl Validate for StaticBearerValidator {
    async fn validate(&self, token: &str) -> AuthResult {
        if self.valid_tokens.contains(token) {
            AuthResult::Authenticated(Some(AuthInfo {
                client_id: format!("bearer:{}", &token[..8.min(token.len())]),
                claims: None,
            }))
        } else {
            AuthResult::Failed(AuthError {
                code: "invalid_token".to_string(),
                message: "The provided bearer token is not valid".to_string(),
            })
        }
    }
}

// =============================================================================
// Authorization Header Parsing
// =============================================================================

/// Extract an API key from an Authorization header
///
/// Supports formats:
/// - `Bearer <key>` (standard)
/// - `ApiKey <key>`
/// - `<key>` (raw key)
pub fn extract_api_key(auth_header: &str) -> Option<&str> {
    let auth_header = auth_header.trim();

    if let Some(key) = auth_header.strip_prefix("Bearer ") {
        Some(key.trim())
    } else if let Some(key) = auth_header.strip_prefix("ApiKey ") {
        Some(key.trim())
    } else if !auth_header.contains(' ') {
        // Raw key without prefix
        Some(auth_header)
    } else {
        None
    }
}

/// Extract a bearer token from an Authorization header
pub fn extract_bearer_token(auth_header: &str) -> Option<&str> {
    auth_header.trim().strip_prefix("Bearer ").map(|t| t.trim())
}

// =============================================================================
// Generic Auth Layer
// =============================================================================

/// A Tower layer that performs authentication using a provided validator
///
/// This is a generic auth layer that can be used with any validator that
/// implements the appropriate validation trait.
#[derive(Clone)]
pub struct AuthLayer<V> {
    validator: V,
    header_name: String,
}

impl<V> AuthLayer<V> {
    /// Create a new auth layer with the given validator
    ///
    /// By default, looks for the `Authorization` header
    pub fn new(validator: V) -> Self {
        Self {
            validator,
            header_name: "Authorization".to_string(),
        }
    }

    /// Use a custom header name for the auth token
    pub fn header_name(mut self, name: impl Into<String>) -> Self {
        self.header_name = name.into();
        self
    }
}

impl<S, V: Clone> Layer<S> for AuthLayer<V> {
    type Service = AuthService<S, V>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthService {
            inner,
            validator: self.validator.clone(),
            header_name: self.header_name.clone(),
        }
    }
}

/// Tower service that performs authentication on incoming requests.
///
/// Created by [`AuthLayer`]. Extracts credentials from the configured HTTP
/// header, validates them using the provided [`Validate`] implementation,
/// and either forwards the request (injecting [`AuthInfo`] into request
/// extensions) or returns an HTTP 401 response.
///
/// # Example
///
/// ```rust,ignore
/// // Requires the `http` feature
/// use tower::ServiceBuilder;
/// use tower_mcp::auth::{AuthLayer, ApiKeyValidator};
///
/// let validator = ApiKeyValidator::new(vec!["sk-test-key-123".to_string()]);
///
/// let service = ServiceBuilder::new()
///     .layer(AuthLayer::new(validator))
///     .service(inner_service);
/// ```
#[derive(Clone)]
pub struct AuthService<S, V> {
    inner: S,
    validator: V,
    header_name: String,
}

#[cfg(feature = "http")]
impl<S, V> Service<axum::http::Request<axum::body::Body>> for AuthService<S, V>
where
    S: Service<axum::http::Request<axum::body::Body>, Response = axum::response::Response>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
    V: Validate,
{
    type Response = axum::response::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::http::Request<axum::body::Body>) -> Self::Future {
        let credential = req
            .headers()
            .get(&self.header_name)
            .and_then(|v| v.to_str().ok())
            .and_then(extract_api_key)
            .map(|s| s.to_owned());

        let mut inner = self.inner.clone();
        let validator = self.validator.clone();

        Box::pin(async move {
            let Some(credential) = credential else {
                return Ok(unauthorized_response(
                    "Missing authentication credentials. Provide via Authorization header.",
                ));
            };

            match validator.validate(&credential).await {
                AuthResult::Authenticated(info) => {
                    let mut req = req;
                    if let Some(info) = info {
                        req.extensions_mut().insert(info);
                    }
                    inner.call(req).await
                }
                AuthResult::Failed(err) => Ok(unauthorized_response(&err.message)),
            }
        })
    }
}

/// Construct an HTTP 401 Unauthorized response with a JSON-RPC error body.
#[cfg(feature = "http")]
fn unauthorized_response(message: &str) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "error": {
            "code": -32001,
            "message": message
        },
        "id": null
    });

    (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response()
}

// =============================================================================
// Helper for building auth middleware
// =============================================================================

/// Builder for creating auth middleware configurations
#[derive(Clone)]
pub struct AuthConfig {
    /// Whether to allow unauthenticated requests to pass through
    pub allow_anonymous: bool,
    /// Paths that don't require authentication
    pub public_paths: Vec<String>,
    /// Custom header name for auth token
    pub header_name: String,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            allow_anonymous: false,
            public_paths: Vec::new(),
            header_name: "Authorization".to_string(),
        }
    }
}

impl AuthConfig {
    /// Create a new auth config
    pub fn new() -> Self {
        Self::default()
    }

    /// Allow anonymous requests (no auth required)
    pub fn allow_anonymous(mut self, allow: bool) -> Self {
        self.allow_anonymous = allow;
        self
    }

    /// Add paths that don't require authentication
    pub fn public_path(mut self, path: impl Into<String>) -> Self {
        self.public_paths.push(path.into());
        self
    }

    /// Set the header name for auth tokens
    pub fn header_name(mut self, name: impl Into<String>) -> Self {
        self.header_name = name.into();
        self
    }

    /// Check if a path is public (doesn't require auth)
    pub fn is_public(&self, path: &str) -> bool {
        self.public_paths.iter().any(|p| path.starts_with(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_api_key_bearer() {
        assert_eq!(extract_api_key("Bearer sk-123"), Some("sk-123"));
        assert_eq!(extract_api_key("Bearer  sk-123 "), Some("sk-123"));
    }

    #[test]
    fn test_extract_api_key_apikey_prefix() {
        assert_eq!(extract_api_key("ApiKey sk-123"), Some("sk-123"));
    }

    #[test]
    fn test_extract_api_key_raw() {
        assert_eq!(extract_api_key("sk-123"), Some("sk-123"));
    }

    #[test]
    fn test_extract_api_key_invalid() {
        assert_eq!(extract_api_key("Basic user:pass"), None);
    }

    #[test]
    fn test_extract_bearer_token() {
        assert_eq!(extract_bearer_token("Bearer abc123"), Some("abc123"));
        assert_eq!(extract_bearer_token("bearer abc123"), None); // case sensitive
        assert_eq!(extract_bearer_token("abc123"), None);
    }

    #[tokio::test]
    async fn test_api_key_validator() {
        let validator = ApiKeyValidator::new(vec!["valid-key".to_string()]);

        match validator.validate("valid-key").await {
            AuthResult::Authenticated(info) => {
                assert!(info.is_some());
            }
            AuthResult::Failed(_) => panic!("Expected authentication to succeed"),
        }

        match validator.validate("invalid-key").await {
            AuthResult::Authenticated(_) => panic!("Expected authentication to fail"),
            AuthResult::Failed(err) => {
                assert_eq!(err.code, "invalid_api_key");
            }
        }
    }

    #[tokio::test]
    async fn test_bearer_validator() {
        let validator = StaticBearerValidator::new(vec!["token123".to_string()]);

        match validator.validate("token123").await {
            AuthResult::Authenticated(info) => {
                assert!(info.is_some());
            }
            AuthResult::Failed(_) => panic!("Expected authentication to succeed"),
        }

        match validator.validate("bad-token").await {
            AuthResult::Authenticated(_) => panic!("Expected authentication to fail"),
            AuthResult::Failed(err) => {
                assert_eq!(err.code, "invalid_token");
            }
        }
    }

    #[test]
    fn test_auth_config() {
        let config = AuthConfig::new()
            .allow_anonymous(false)
            .public_path("/health")
            .public_path("/metrics")
            .header_name("X-API-Key");

        assert!(!config.allow_anonymous);
        assert!(config.is_public("/health"));
        assert!(config.is_public("/metrics/cpu"));
        assert!(!config.is_public("/api/tools"));
        assert_eq!(config.header_name, "X-API-Key");
    }

    #[test]
    fn test_auth_layer_creates_service() {
        let validator = ApiKeyValidator::new(vec!["key".to_string()]);
        let layer = AuthLayer::new(validator);
        // Wrap a no-op service to verify the Layer impl works
        let _service: AuthService<(), ApiKeyValidator> = layer.layer(());
    }

    #[cfg(feature = "http")]
    mod http_tests {
        use super::*;
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        /// A minimal inner service that returns 200 OK for any request
        #[derive(Clone)]
        struct OkService;

        impl Service<Request<Body>> for OkService {
            type Response = axum::response::Response;
            type Error = std::convert::Infallible;
            type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, _req: Request<Body>) -> Self::Future {
                Box::pin(async {
                    Ok(axum::response::Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::empty())
                        .unwrap())
                })
            }
        }

        #[tokio::test]
        async fn test_auth_service_rejects_missing_credentials() {
            let validator = ApiKeyValidator::new(vec!["sk-test-123".to_string()]);
            let layer = AuthLayer::new(validator);
            let mut service = layer.layer(OkService);

            let req = Request::builder().uri("/").body(Body::empty()).unwrap();

            let resp = service.ready().await.unwrap().call(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }

        #[tokio::test]
        async fn test_auth_service_rejects_invalid_key() {
            let validator = ApiKeyValidator::new(vec!["sk-test-123".to_string()]);
            let layer = AuthLayer::new(validator);
            let mut service = layer.layer(OkService);

            let req = Request::builder()
                .uri("/")
                .header("Authorization", "Bearer sk-wrong-key")
                .body(Body::empty())
                .unwrap();

            let resp = service.ready().await.unwrap().call(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }

        #[tokio::test]
        async fn test_auth_service_accepts_valid_key() {
            let validator = ApiKeyValidator::new(vec!["sk-test-123".to_string()]);
            let layer = AuthLayer::new(validator);
            let mut service = layer.layer(OkService);

            let req = Request::builder()
                .uri("/")
                .header("Authorization", "Bearer sk-test-123")
                .body(Body::empty())
                .unwrap();

            let resp = service.ready().await.unwrap().call(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        #[tokio::test]
        async fn test_auth_service_injects_auth_info() {
            let validator = ApiKeyValidator::new(vec!["sk-test-123".to_string()]);
            let layer = AuthLayer::new(validator);

            // Inner service that checks for AuthInfo in extensions
            #[derive(Clone)]
            struct CheckAuthInfo;

            impl Service<Request<Body>> for CheckAuthInfo {
                type Response = axum::response::Response;
                type Error = std::convert::Infallible;
                type Future =
                    Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

                fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                    Poll::Ready(Ok(()))
                }

                fn call(&mut self, req: Request<Body>) -> Self::Future {
                    let has_auth = req.extensions().get::<AuthInfo>().is_some();
                    Box::pin(async move {
                        let status = if has_auth {
                            StatusCode::OK
                        } else {
                            StatusCode::INTERNAL_SERVER_ERROR
                        };
                        Ok(axum::response::Response::builder()
                            .status(status)
                            .body(Body::empty())
                            .unwrap())
                    })
                }
            }

            let mut service = layer.layer(CheckAuthInfo);

            let req = Request::builder()
                .uri("/")
                .header("Authorization", "Bearer sk-test-123")
                .body(Body::empty())
                .unwrap();

            let resp = service.ready().await.unwrap().call(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        #[tokio::test]
        async fn test_auth_service_custom_header() {
            let validator = ApiKeyValidator::new(vec!["my-key".to_string()]);
            let layer = AuthLayer::new(validator).header_name("X-API-Key");
            let mut service = layer.layer(OkService);

            // Standard Authorization header should not work
            let req = Request::builder()
                .uri("/")
                .header("Authorization", "Bearer my-key")
                .body(Body::empty())
                .unwrap();
            let resp = service.ready().await.unwrap().call(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

            // Custom header should work
            let req = Request::builder()
                .uri("/")
                .header("X-API-Key", "my-key")
                .body(Body::empty())
                .unwrap();
            let resp = service.ready().await.unwrap().call(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
    }
}
