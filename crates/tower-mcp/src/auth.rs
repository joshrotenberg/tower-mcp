//! Header-credential authentication for HTTP MCP servers.
//!
//! These helpers cover the case where the credential is a shared secret the
//! server can check against something it already holds: an API key, or a
//! bearer token drawn from a fixed set. Write the check as a [`Validate`]
//! implementation and install it with [`AuthLayer`].
//!
//! For OAuth 2.1 resource-server behavior, reach for [`crate::oauth`] instead.
//! It validates JWT signatures and audiences, enforces scopes, serves
//! Protected Resource Metadata, and answers with the `WWW-Authenticate`
//! challenge an OAuth client needs in order to recover. It also bridges the
//! token's `sub` claim into the request extensions, which is what binds an
//! async task to the principal that created it. Nothing in this module
//! populates that claim, so on a server authenticated with an [`AuthLayer`]
//! every task is unowned (see [`crate::async_task`]).
//!
//! # Where the layer belongs
//!
//! [`AuthLayer`] operates on the HTTP request, not on a decoded MCP request,
//! so it wraps the transport rather than being installed inside it with
//! [`HttpTransport::layer`]. A request with no acceptable credential is
//! refused before its JSON-RPC body is parsed, so no handler ever runs for it,
//! and the rejection costs nothing but the header read.
//!
//! ```rust
//! # #[tokio::main]
//! # async fn main() {
//! # #[cfg(feature = "http")]
//! # {
//! use tower_mcp::auth::{ApiKeyValidator, AuthLayer};
//! use tower_mcp::transport::HttpTransport;
//! use tower_mcp::McpRouter;
//!
//! let validator = ApiKeyValidator::new(["sk-live-1".to_string()]);
//! let router = McpRouter::new().server_info("my-server", "1.0.0");
//!
//! // `into_router` hands back a plain axum router, so the auth layer goes on
//! // the outside like any other HTTP middleware.
//! let app = HttpTransport::new(router)
//!     .into_router()
//!     .layer(AuthLayer::new(validator));
//! # let _ = app;
//! # }
//! # }
//! ```
//!
//! # What a rejection looks like
//!
//! A missing credential and a rejected one both produce HTTP 401 carrying a
//! JSON-RPC error body; only the message differs. The body's code is
//! [`McpErrorCode::Forbidden`] (-32007) rather than the -32001 earlier
//! versions used, because SEP-2243 reclaimed -32001 for `HeaderMismatch` and
//! clients route on the code.
//!
//! [`HttpTransport::layer`]: crate::transport::HttpTransport::layer
//! [`McpErrorCode::Forbidden`]: crate::error::McpErrorCode::Forbidden

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use tower::Layer;
#[cfg(feature = "http")]
use tower::ServiceExt;

/// Result of an authentication attempt
///
/// Returned by [`Validate::validate`] and consumed by [`AuthService`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AuthResult {
    /// Authentication succeeded with optional user/client info.
    ///
    /// The [`AuthInfo`], when present, is inserted into the request's
    /// extensions for downstream services to read. `None` admits the request
    /// without recording who sent it, which is worth avoiding on anything
    /// that later wants to attribute an action.
    Authenticated(Option<AuthInfo>),
    /// Authentication failed with a reason
    Failed(AuthError),
}

/// Information about an authenticated client
///
/// [`AuthService`] inserts this into the HTTP request extensions, so an inner
/// service reads it with `req.extensions().get::<AuthInfo>()`. It does not
/// reach an MCP handler's [`RequestContext`](crate::context::RequestContext):
/// only the OAuth path bridges a principal that far.
#[derive(Debug, Clone)]
pub struct AuthInfo {
    /// Client/user identifier
    pub client_id: String,
    /// Optional additional claims or metadata
    pub claims: Option<serde_json::Value>,
}

/// Authentication error
///
/// The `code` is for the server's own logs and metrics; it is not the
/// JSON-RPC error code the client sees, which is always -32007. `message` is
/// copied into the 401 body verbatim, so it should say what the caller can act
/// on and nothing about why the check failed internally.
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
/// value extracted from the configured request header after parsing by
/// [`extract_api_key`], so an implementation sees `sk-123` whether the header
/// read `Bearer sk-123`, `ApiKey sk-123`, or `sk-123`.
///
/// The bound is `Clone + Send + Sync + 'static` because [`AuthService`] clones
/// the validator for every request. Keep any shared state behind an [`Arc`],
/// as [`ApiKeyValidator`] does, so cloning stays cheap.
///
/// # Example
///
/// A validator that looks the credential up in a store, and reports who the
/// caller is so the inner service can attribute the request:
///
/// ```rust
/// use std::collections::HashMap;
/// use std::sync::Arc;
///
/// use tower_mcp::auth::{AuthError, AuthInfo, AuthResult, Validate};
///
/// #[derive(Clone)]
/// struct TenantKeys {
///     // Cheap to clone: the map is shared, not copied, per request.
///     keys: Arc<HashMap<String, String>>,
/// }
///
/// impl Validate for TenantKeys {
///     async fn validate(&self, credential: &str) -> AuthResult {
///         match self.keys.get(credential) {
///             Some(tenant) => AuthResult::Authenticated(Some(AuthInfo {
///                 client_id: tenant.clone(),
///                 claims: None,
///             })),
///             // The message is copied into the 401 body, so it says what to
///             // do rather than which lookup missed.
///             None => AuthResult::Failed(AuthError {
///                 code: "invalid_api_key".to_string(),
///                 message: "Unknown API key".to_string(),
///             }),
///         }
///     }
/// }
///
/// # #[tokio::main]
/// # async fn main() {
/// let validator = TenantKeys {
///     keys: Arc::new(HashMap::from([("sk-live-1".to_string(), "acme".to_string())])),
/// };
///
/// let AuthResult::Authenticated(Some(info)) = validator.validate("sk-live-1").await else {
///     panic!("a known key authenticates");
/// };
/// assert_eq!(info.client_id, "acme");
///
/// assert!(matches!(
///     validator.validate("sk-unknown").await,
///     AuthResult::Failed(_)
/// ));
/// # }
/// ```
///
/// [`Arc`]: std::sync::Arc
pub trait Validate: Clone + Send + Sync + 'static {
    /// Validate a credential and return the authentication result.
    fn validate(&self, credential: &str) -> impl Future<Output = AuthResult> + Send;
}

// =============================================================================
// API Key Authentication
// =============================================================================

/// Simple in-memory API key validator
///
/// The key set is fixed at construction and shared by every clone, which is
/// what makes it cheap enough for [`AuthService`] to clone per request.
///
/// For production use, consider:
/// - Database-backed validation
/// - Caching with TTL
/// - Rate limiting per key
///
/// The [`AuthInfo`] it reports names the caller as `api_key:` followed by the
/// first eight characters of the key, so a log line identifies which key was
/// used without recording the secret.
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
    ///
    /// The set is copy-on-write, so this affects only the validator it is
    /// called on. A validator already handed to [`AuthLayer::new`] was cloned
    /// into the layer, and the running service keeps serving the set it was
    /// built with. Rotating keys on a live server therefore needs a validator
    /// that reads shared mutable state, not this method.
    ///
    /// ```rust
    /// use tower_mcp::auth::ApiKeyValidator;
    ///
    /// let installed = ApiKeyValidator::new(["sk-live-1".to_string()]);
    /// let mut local = installed.clone();
    /// local.add_key("sk-live-2".to_string());
    ///
    /// assert!(local.is_valid("sk-live-2"));
    /// assert!(
    ///     !installed.is_valid("sk-live-2"),
    ///     "the clone diverged; the already-installed validator did not change"
    /// );
    /// ```
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
/// An exact match against an in-memory set: it does not decode the token, so
/// it cannot notice an expiry, an audience, or a scope. That makes it suitable
/// for development, tests, and machine-to-machine links where the token is a
/// shared secret you rotate by restarting.
///
/// For production, implement [`Validate`] with:
/// - JWT verification using a signing key
/// - OAuth2 token introspection
/// - OIDC ID token validation
///
/// [`crate::oauth`] already does the first of those, including audience checks
/// and the `WWW-Authenticate` challenge an OAuth client needs to recover.
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

/// Strip an auth scheme prefix, comparing the scheme case-insensitively.
///
/// RFC 7235 defines the scheme as a case-insensitive token, so `bearer`,
/// `Bearer`, and `BEARER` are the same scheme. Comparing bytes directly keeps
/// this allocation-free, and the token after the space is returned untouched
/// because only the scheme is case-insensitive, never the credential (#1276).
fn strip_scheme<'a>(header: &'a str, scheme: &str) -> Option<&'a str> {
    let rest = header.get(..scheme.len())?;
    if !rest.eq_ignore_ascii_case(scheme) {
        return None;
    }
    let after = header.get(scheme.len()..)?;
    // The scheme must be followed by whitespace, or `Bearerish` would match
    // `Bearer`.
    if !after.starts_with(' ') {
        return None;
    }
    Some(after.trim_start())
}

/// Extract an API key from an Authorization header
///
/// Supports formats:
/// - `Bearer <key>` (standard)
/// - `ApiKey <key>`
/// - `<key>` (raw key)
///
/// This is what [`AuthService`] applies to whichever header
/// [`AuthLayer::header_name`] names, so a custom header accepts all three
/// forms too.
///
/// Two consequences are easy to miss. Any value with no spaces is taken as a
/// raw key, so a header carrying something that is not a credential at all is
/// still handed to the validator to reject. And the scheme match is
/// case-sensitive, so `bearer <key>` is not recognized: it has a space but no
/// known prefix, which reads as no credential.
///
/// ```rust
/// use tower_mcp::auth::extract_api_key;
///
/// assert_eq!(extract_api_key("Bearer sk-123"), Some("sk-123"));
/// assert_eq!(extract_api_key("ApiKey sk-123"), Some("sk-123"));
/// assert_eq!(extract_api_key("sk-123"), Some("sk-123"));
///
/// assert_eq!(extract_api_key("Basic dXNlcjpwYXNz"), None);
///
/// // RFC 7235 makes the scheme case-insensitive; the token is not.
/// assert_eq!(extract_api_key("bearer sk-123"), Some("sk-123"));
/// assert_eq!(extract_api_key("BEARER sk-123"), Some("sk-123"));
/// ```
pub fn extract_api_key(auth_header: &str) -> Option<&str> {
    let auth_header = auth_header.trim();

    if let Some(key) = strip_scheme(auth_header, "Bearer") {
        Some(key.trim())
    } else if let Some(key) = strip_scheme(auth_header, "ApiKey") {
        Some(key.trim())
    } else if !auth_header.contains(' ') {
        // Raw key without prefix
        Some(auth_header)
    } else {
        None
    }
}

/// Extract a bearer token from an Authorization header
///
/// Stricter than [`extract_api_key`]: only the `Bearer` scheme is accepted,
/// and a bare value with no scheme is rejected rather than treated as a raw
/// token. The scheme itself is matched case-insensitively, as RFC 7235
/// requires; the token is not.
///
/// ```rust
/// use tower_mcp::auth::extract_bearer_token;
///
/// assert_eq!(extract_bearer_token("Bearer abc123"), Some("abc123"));
/// assert_eq!(extract_bearer_token("bearer abc123"), Some("abc123"));
///
/// // A bare value is not a bearer token, and a longer scheme is not `Bearer`.
/// assert_eq!(extract_bearer_token("abc123"), None);
/// assert_eq!(extract_bearer_token("Bearerish abc123"), None);
/// ```
pub fn extract_bearer_token(auth_header: &str) -> Option<&str> {
    strip_scheme(auth_header.trim(), "Bearer").map(|t| t.trim())
}

// =============================================================================
// Generic Auth Layer
// =============================================================================

/// A Tower layer that performs authentication using a provided validator
///
/// Wraps an HTTP service with any [`Validate`] implementation: the credential
/// is read from one header, checked, and either the request continues with an
/// [`AuthInfo`] in its extensions or it is answered with 401 without reaching
/// the inner service.
///
/// The layer itself imposes no bounds, so it can be constructed in any build.
/// The [`Service`](tower_service::Service) implementation it produces exists
/// only with the `http` feature, since that is what supplies the request and
/// response types.
///
/// # Example
///
/// ```rust
/// # #[tokio::main]
/// # async fn main() {
/// # #[cfg(feature = "http")]
/// # {
/// use axum::body::Body;
/// use axum::http::{Request, StatusCode};
/// use axum::response::Response;
/// use tower::{Layer, ServiceExt};
/// use tower_mcp::auth::{ApiKeyValidator, AuthInfo, AuthLayer};
///
/// // Stands in for the transport. It answers 200 only when the layer named
/// // the caller, so the assertions below also prove the extension is set.
/// let inner = tower::service_fn(|req: Request<Body>| async move {
///     let status = match req.extensions().get::<AuthInfo>() {
///         Some(_) => StatusCode::OK,
///         None => StatusCode::INTERNAL_SERVER_ERROR,
///     };
///     Ok::<_, std::convert::Infallible>(
///         Response::builder().status(status).body(Body::empty()).unwrap(),
///     )
/// });
///
/// let service = AuthLayer::new(ApiKeyValidator::new(["sk-live-1".to_string()])).layer(inner);
///
/// let no_credential = Request::builder().uri("/mcp").body(Body::empty()).unwrap();
/// let response = service.clone().oneshot(no_credential).await.unwrap();
/// assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
///
/// let wrong_key = Request::builder()
///     .uri("/mcp")
///     .header("Authorization", "Bearer sk-not-issued")
///     .body(Body::empty())
///     .unwrap();
/// let response = service.clone().oneshot(wrong_key).await.unwrap();
/// assert_eq!(
///     response.status(),
///     StatusCode::UNAUTHORIZED,
///     "a rejected credential is answered exactly like a missing one"
/// );
///
/// let good_key = Request::builder()
///     .uri("/mcp")
///     .header("Authorization", "Bearer sk-live-1")
///     .body(Body::empty())
///     .unwrap();
/// let response = service.oneshot(good_key).await.unwrap();
/// assert_eq!(response.status(), StatusCode::OK);
/// # }
/// # }
/// ```
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
    ///
    /// Only where the credential is read from changes. The value is still
    /// parsed by [`extract_api_key`], so `X-API-Key: sk-1` and
    /// `X-API-Key: Bearer sk-1` are both accepted and both reach the validator
    /// as `sk-1`.
    ///
    /// The named header becomes the only one consulted: an `Authorization`
    /// header alongside it is ignored, and a request carrying only
    /// `Authorization` is refused as though it carried no credential at all.
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
/// Created by [`AuthLayer`], which carries the worked example. Extracts the
/// credential from the configured HTTP header with [`extract_api_key`],
/// validates it with the provided [`Validate`] implementation, and either
/// forwards the request or answers 401 without calling the inner service.
///
/// Three details are worth knowing before relying on it:
///
/// - A missing credential is refused without consulting the validator, so a
///   [`Validate`] implementation never sees an empty string and cannot choose
///   to admit anonymous callers. Use [`AuthLayer`] only on routes that must be
///   authenticated, and leave public routes outside it.
/// - [`AuthResult::Authenticated(None)`](AuthResult::Authenticated) forwards
///   the request with no [`AuthInfo`] in its extensions, so the inner service
///   cannot tell it apart from an unauthenticated one it never sees.
/// - `poll_ready` delegates to the inner service, so backpressure is the inner
///   service's, unchanged. Validation itself runs in the returned future, not
///   in `poll_ready`.
#[derive(Clone)]
#[cfg_attr(not(feature = "http"), allow(dead_code))]
pub struct AuthService<S, V> {
    inner: S,
    validator: V,
    header_name: String,
}

#[cfg(feature = "http")]
impl<S, V> tower_service::Service<axum::http::Request<axum::body::Body>> for AuthService<S, V>
where
    S: tower_service::Service<
            axum::http::Request<axum::body::Body>,
            Response = axum::response::Response,
        > + Clone
        + Send
        + 'static,
    S::Future: Send,
    S::Error: Into<crate::BoxError> + Send,
    V: Validate,
{
    type Response = axum::response::Response;
    type Error = S::Error;
    type Future =
        std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::http::Request<axum::body::Body>) -> Self::Future {
        let credential = req
            .headers()
            .get(&self.header_name)
            .and_then(|v| v.to_str().ok())
            .and_then(extract_api_key)
            .map(|s| s.to_owned());

        let inner = self.inner.clone();
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
                    inner.oneshot(req).await
                }
                AuthResult::Failed(err) => Ok(unauthorized_response(&err.message)),
            }
        })
    }
}

/// Construct an HTTP 401 Unauthorized response with a JSON-RPC error body.
///
/// Uses the MCP `Forbidden` code (-32007). The previous code (-32001) was
/// reclaimed by SEP-2243 for `HeaderMismatch`; emitting that here would
/// confuse clients that route on the JSON-RPC error code.
#[cfg(feature = "http")]
fn unauthorized_response(message: &str) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "error": {
            "code": tower_mcp_types::McpErrorCode::Forbidden.code(),
            "message": message
        },
        "id": null
    });

    (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response()
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

    /// RFC 7235 defines the auth scheme as case-insensitive. This test used
    /// to assert the opposite, which is what made a client sending
    /// `authorization: bearer ...` get a 401 for no visible reason (#1276).
    #[test]
    fn test_extract_bearer_token() {
        assert_eq!(extract_bearer_token("Bearer abc123"), Some("abc123"));
        assert_eq!(extract_bearer_token("bearer abc123"), Some("abc123"));
        assert_eq!(extract_bearer_token("BEARER abc123"), Some("abc123"));
        assert_eq!(extract_bearer_token("BeArEr abc123"), Some("abc123"));

        // The token itself is never case-folded.
        assert_eq!(extract_bearer_token("Bearer AbC123"), Some("AbC123"));

        // A bare value is still not a bearer token, and a scheme that merely
        // starts with the right letters is not the right scheme.
        assert_eq!(extract_bearer_token("abc123"), None);
        assert_eq!(extract_bearer_token("Bearerish abc123"), None);
        assert_eq!(extract_bearer_token("Basic abc123"), None);
    }

    /// The same rule on the more permissive extractor, including its raw-key
    /// fallback which must keep working.
    #[test]
    fn extract_api_key_matches_the_scheme_case_insensitively() {
        for header in ["Bearer sk-1", "bearer sk-1", "BEARER sk-1"] {
            assert_eq!(extract_api_key(header), Some("sk-1"), "for {header}");
        }
        for header in ["ApiKey sk-1", "apikey sk-1", "APIKEY sk-1"] {
            assert_eq!(extract_api_key(header), Some("sk-1"), "for {header}");
        }
        assert_eq!(extract_api_key("sk-1"), Some("sk-1"), "raw key still works");
        assert_eq!(extract_api_key("Basic dXNlcjpwYXNz"), None);
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
    fn test_auth_layer_creates_service() {
        let validator = ApiKeyValidator::new(vec!["key".to_string()]);
        let layer = AuthLayer::new(validator);
        // Wrap a no-op service to verify the Layer impl works
        let _service: AuthService<(), ApiKeyValidator> = layer.layer(());
    }

    #[cfg(feature = "http")]
    mod http_tests {
        use super::*;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;
        use tower_service::Service;

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
