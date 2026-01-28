//! Authentication middleware helpers for MCP servers
//!
//! This module provides helper types and layers for common authentication patterns.
//! Since tower-mcp is built on Tower, standard tower middleware can be used directly.
//!
//! # Patterns
//!
//! ## API Key Authentication
//!
//! ```rust,no_run
//! use tower_mcp::auth::{ApiKeyLayer, ApiKeyValidator};
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
//! // The ApiKeyLayer extracts the key from the Authorization header
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
use std::sync::Arc;

use tower::Layer;

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
// API Key Authentication
// =============================================================================

/// Trait for validating API keys
pub trait ApiKeyValidation: Clone + Send + Sync + 'static {
    /// Validate an API key and return auth info if valid
    fn validate(&self, key: &str) -> impl Future<Output = AuthResult> + Send;
}

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

impl ApiKeyValidation for ApiKeyValidator {
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

/// Trait for validating bearer tokens (OAuth2, JWT, etc.)
pub trait BearerTokenValidation: Clone + Send + Sync + 'static {
    /// Validate a bearer token and return auth info if valid
    fn validate(&self, token: &str) -> impl Future<Output = AuthResult> + Send;
}

/// Simple bearer token validator that checks against a static set of tokens
///
/// For production, implement `BearerTokenValidation` with:
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

impl BearerTokenValidation for StaticBearerValidator {
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

/// The service created by `AuthLayer`
///
/// Note: The actual Service implementation depends on the request/response
/// types being used. For HTTP, this would typically be implemented for
/// axum's Request/Response types. See the examples for concrete implementations.
#[derive(Clone)]
pub struct AuthService<S, V> {
    /// The wrapped inner service
    pub inner: S,
    /// The validator used for authentication
    pub validator: V,
    /// The header name to extract auth tokens from
    pub header_name: String,
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
}
