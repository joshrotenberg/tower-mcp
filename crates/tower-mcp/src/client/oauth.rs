//! OAuth 2.0 Client Credentials grant for machine-to-machine authentication.
//!
//! Provides [`OAuthClientCredentials`] for acquiring and caching access tokens
//! using the [client credentials grant](https://datatracker.ietf.org/doc/html/rfc6749#section-4.4).
//! Tokens are cached in memory and refreshed automatically before expiry.
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::client::{HttpClientTransport, OAuthClientCredentials};
//!
//! # async fn example() -> Result<(), tower_mcp::BoxError> {
//! // Direct token endpoint
//! let provider = OAuthClientCredentials::builder()
//!     .client_id("my-client")
//!     .client_secret("my-secret")
//!     .token_endpoint("https://auth.example.com/token")
//!     .scopes(["mcp:tools", "mcp:resources"])
//!     .build()?;
//!
//! let transport = HttpClientTransport::new("http://localhost:3000")
//!     .with_token_provider(provider);
//! # Ok(())
//! # }
//! ```
//!
//! # Auth Server Discovery
//!
//! ```rust,no_run
//! use tower_mcp::client::OAuthClientCredentials;
//!
//! # async fn example() -> Result<(), tower_mcp::BoxError> {
//! let provider = OAuthClientCredentials::discover(
//!     "https://auth.example.com",
//!     "my-client",
//!     "my-secret",
//! ).await?;
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::RwLock;

/// Trait for dynamic token providers.
///
/// Implement this to provide bearer tokens that are acquired and refreshed
/// at runtime (e.g., via OAuth 2.0 grants).
///
/// # Example
///
/// ```rust,no_run
/// use async_trait::async_trait;
/// use tower_mcp::client::{TokenProvider, OAuthClientError};
///
/// struct StaticTokenProvider(String);
///
/// #[async_trait]
/// impl TokenProvider for StaticTokenProvider {
///     async fn get_token(&self) -> Result<String, OAuthClientError> {
///         Ok(self.0.clone())
///     }
/// }
/// ```
#[async_trait]
pub trait TokenProvider: Send + Sync + 'static {
    /// Get a valid bearer token.
    ///
    /// Implementations should cache tokens and return cached values when
    /// still valid. This method may be called concurrently from multiple
    /// tasks.
    async fn get_token(&self) -> Result<String, OAuthClientError>;
}

/// Error type for OAuth client operations.
#[derive(Debug)]
#[non_exhaustive]
pub enum OAuthClientError {
    /// Failed to discover the authorization server metadata.
    Discovery(String),
    /// Failed to request a token from the token endpoint.
    TokenRequest(String),
    /// The token response was invalid or missing required fields.
    InvalidResponse(String),
    /// Builder validation failed (missing required fields).
    BuildError(String),
}

impl fmt::Display for OAuthClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Discovery(msg) => write!(f, "OAuth discovery error: {}", msg),
            Self::TokenRequest(msg) => write!(f, "OAuth token request error: {}", msg),
            Self::InvalidResponse(msg) => write!(f, "OAuth invalid response: {}", msg),
            Self::BuildError(msg) => write!(f, "OAuth builder error: {}", msg),
        }
    }
}

impl std::error::Error for OAuthClientError {}

/// A cached access token with its expiration time.
#[derive(Debug, Clone)]
struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

/// OAuth 2.0 token response (subset of RFC 6749 Section 5.1).
#[derive(Debug, serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    #[allow(dead_code)]
    token_type: String,
    /// Token lifetime in seconds.
    expires_in: Option<u64>,
    #[allow(dead_code)]
    scope: Option<String>,
}

/// Authorization server metadata response (RFC 8414).
#[derive(Debug, serde::Deserialize)]
struct AuthServerMetadata {
    token_endpoint: String,
}

/// Shared state for [`OAuthClientCredentials`].
struct OAuthClientCredentialsInner {
    client_id: String,
    client_secret: String,
    token_endpoint: String,
    scopes: Option<String>,
    refresh_buffer: Duration,
    client: reqwest::Client,
    cache: RwLock<Option<CachedToken>>,
}

/// OAuth 2.0 Client Credentials token provider.
///
/// Acquires access tokens using the
/// [client credentials grant](https://datatracker.ietf.org/doc/html/rfc6749#section-4.4)
/// and caches them until expiry. Uses `client_secret_basic` authentication
/// (HTTP Basic with `client_id:client_secret`).
///
/// # Token Caching
///
/// Tokens are cached in an `RwLock` and refreshed when they expire
/// (minus a configurable buffer, default 30 seconds). Concurrent
/// callers share the read lock; only one caller performs the refresh
/// while others wait on the write lock, then re-verify (double-check
/// pattern to prevent thundering herd).
///
/// # Example
///
/// ```rust,no_run
/// use tower_mcp::client::OAuthClientCredentials;
/// use std::time::Duration;
///
/// # fn example() -> Result<(), tower_mcp::BoxError> {
/// let provider = OAuthClientCredentials::builder()
///     .client_id("my-service")
///     .client_secret("s3cret")
///     .token_endpoint("https://auth.example.com/oauth/token")
///     .scopes(["mcp:tools"])
///     .refresh_buffer(Duration::from_secs(60))
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct OAuthClientCredentials {
    inner: Arc<OAuthClientCredentialsInner>,
}

impl fmt::Debug for OAuthClientCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthClientCredentials")
            .field("client_id", &self.inner.client_id)
            .field("token_endpoint", &self.inner.token_endpoint)
            .field("scopes", &self.inner.scopes)
            .field("refresh_buffer", &self.inner.refresh_buffer)
            .finish()
    }
}

impl OAuthClientCredentials {
    /// Create a builder for configuring the client credentials provider.
    pub fn builder() -> OAuthClientCredentialsBuilder {
        OAuthClientCredentialsBuilder::default()
    }

    /// Discover the token endpoint from the authorization server metadata
    /// and create a provider.
    ///
    /// Fetches `{issuer}/.well-known/oauth-authorization-server` to find
    /// the `token_endpoint`, then creates the provider with default settings.
    ///
    /// # Errors
    ///
    /// Returns [`OAuthClientError::Discovery`] if the metadata endpoint
    /// is unreachable or does not contain a `token_endpoint`.
    pub async fn discover(
        issuer: &str,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Result<Self, OAuthClientError> {
        let client = reqwest::Client::new();
        let url = format!(
            "{}/.well-known/oauth-authorization-server",
            issuer.trim_end_matches('/')
        );

        let metadata: AuthServerMetadata = client
            .get(&url)
            .send()
            .await
            .map_err(|e| OAuthClientError::Discovery(e.to_string()))?
            .json()
            .await
            .map_err(|e| OAuthClientError::Discovery(e.to_string()))?;

        Self::builder()
            .client_id(client_id)
            .client_secret(client_secret)
            .token_endpoint(metadata.token_endpoint)
            .build()
            .map_err(|e| OAuthClientError::Discovery(e.to_string()))
    }

    /// Check if the cached token is still valid (not expired minus buffer).
    fn is_token_valid(token: &CachedToken, buffer: Duration) -> bool {
        token
            .expires_at
            .checked_sub(buffer)
            .is_some_and(|effective| Instant::now() < effective)
    }

    /// Perform the token request to the authorization server.
    async fn fetch_token(&self) -> Result<CachedToken, OAuthClientError> {
        use base64::Engine;

        let credentials = base64::engine::general_purpose::STANDARD.encode(format!(
            "{}:{}",
            self.inner.client_id, self.inner.client_secret
        ));

        let mut body = "grant_type=client_credentials".to_string();
        if let Some(ref scopes) = self.inner.scopes {
            body.push_str("&scope=");
            body.push_str(scopes);
        }

        let response = self
            .inner
            .client
            .post(&self.inner.token_endpoint)
            .header("Authorization", format!("Basic {}", credentials))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| OAuthClientError::TokenRequest(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(OAuthClientError::TokenRequest(format!(
                "HTTP {}: {}",
                status, body
            )));
        }

        let token_response: TokenResponse = response
            .json()
            .await
            .map_err(|e| OAuthClientError::InvalidResponse(e.to_string()))?;

        // Default to 1 hour if expires_in not provided
        let expires_in = Duration::from_secs(token_response.expires_in.unwrap_or(3600));
        let expires_at = Instant::now() + expires_in;

        Ok(CachedToken {
            access_token: token_response.access_token,
            expires_at,
        })
    }
}

#[async_trait]
impl TokenProvider for OAuthClientCredentials {
    async fn get_token(&self) -> Result<String, OAuthClientError> {
        // Fast path: read lock, return cached token if valid
        {
            let cache = self.inner.cache.read().await;
            if let Some(ref token) = *cache
                && Self::is_token_valid(token, self.inner.refresh_buffer)
            {
                return Ok(token.access_token.clone());
            }
        }

        // Slow path: write lock, double-check, then fetch
        let mut cache = self.inner.cache.write().await;

        // Another task may have refreshed while we waited for the lock
        if let Some(ref token) = *cache
            && Self::is_token_valid(token, self.inner.refresh_buffer)
        {
            return Ok(token.access_token.clone());
        }

        let token = self.fetch_token().await?;
        let access_token = token.access_token.clone();
        *cache = Some(token);

        Ok(access_token)
    }
}

/// Builder for [`OAuthClientCredentials`].
///
/// # Required Fields
///
/// - `client_id`
/// - `client_secret`
/// - `token_endpoint`
///
/// # Example
///
/// ```rust,no_run
/// use tower_mcp::client::OAuthClientCredentials;
///
/// # fn example() -> Result<(), tower_mcp::BoxError> {
/// let provider = OAuthClientCredentials::builder()
///     .client_id("my-service")
///     .client_secret("s3cret")
///     .token_endpoint("https://auth.example.com/token")
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Default)]
pub struct OAuthClientCredentialsBuilder {
    client_id: Option<String>,
    client_secret: Option<String>,
    token_endpoint: Option<String>,
    scopes: Option<String>,
    refresh_buffer: Option<Duration>,
    client: Option<reqwest::Client>,
}

impl OAuthClientCredentialsBuilder {
    /// Set the OAuth client ID.
    pub fn client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = Some(client_id.into());
        self
    }

    /// Set the OAuth client secret.
    pub fn client_secret(mut self, client_secret: impl Into<String>) -> Self {
        self.client_secret = Some(client_secret.into());
        self
    }

    /// Set the token endpoint URL.
    pub fn token_endpoint(mut self, url: impl Into<String>) -> Self {
        self.token_endpoint = Some(url.into());
        self
    }

    /// Set the requested scopes.
    ///
    /// Accepts an iterator of scope strings, which are joined with spaces
    /// per RFC 6749 Section 3.3.
    pub fn scopes(mut self, scopes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let scope_str: Vec<String> = scopes.into_iter().map(|s| s.into()).collect();
        if !scope_str.is_empty() {
            self.scopes = Some(scope_str.join(" "));
        }
        self
    }

    /// Set the refresh buffer duration.
    ///
    /// Tokens are refreshed this long before their actual expiry to avoid
    /// using a token that expires during a request. Default: 30 seconds.
    pub fn refresh_buffer(mut self, duration: Duration) -> Self {
        self.refresh_buffer = Some(duration);
        self
    }

    /// Set a custom `reqwest::Client` for token requests.
    ///
    /// Use this when you need custom TLS configuration or proxy settings.
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.client = Some(client);
        self
    }

    /// Build the [`OAuthClientCredentials`] provider.
    ///
    /// # Errors
    ///
    /// Returns [`OAuthClientError::BuildError`] if `client_id`, `client_secret`,
    /// or `token_endpoint` are not set.
    pub fn build(self) -> Result<OAuthClientCredentials, OAuthClientError> {
        let client_id = self
            .client_id
            .ok_or_else(|| OAuthClientError::BuildError("client_id is required".into()))?;
        let client_secret = self
            .client_secret
            .ok_or_else(|| OAuthClientError::BuildError("client_secret is required".into()))?;
        let token_endpoint = self
            .token_endpoint
            .ok_or_else(|| OAuthClientError::BuildError("token_endpoint is required".into()))?;

        let inner = OAuthClientCredentialsInner {
            client_id,
            client_secret,
            token_endpoint,
            scopes: self.scopes,
            refresh_buffer: self.refresh_buffer.unwrap_or(Duration::from_secs(30)),
            client: self.client.unwrap_or_default(),
            cache: RwLock::new(None),
        };

        Ok(OAuthClientCredentials {
            inner: Arc::new(inner),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_builder_missing_client_id() {
        let err = OAuthClientCredentials::builder()
            .client_secret("secret")
            .token_endpoint("https://auth.example.com/token")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("client_id"));
    }

    #[test]
    fn test_builder_missing_client_secret() {
        let err = OAuthClientCredentials::builder()
            .client_id("id")
            .token_endpoint("https://auth.example.com/token")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("client_secret"));
    }

    #[test]
    fn test_builder_missing_token_endpoint() {
        let err = OAuthClientCredentials::builder()
            .client_id("id")
            .client_secret("secret")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("token_endpoint"));
    }

    #[test]
    fn test_builder_success() {
        let provider = OAuthClientCredentials::builder()
            .client_id("my-client")
            .client_secret("my-secret")
            .token_endpoint("https://auth.example.com/token")
            .build()
            .unwrap();

        assert_eq!(provider.inner.client_id, "my-client");
        assert_eq!(
            provider.inner.token_endpoint,
            "https://auth.example.com/token"
        );
        assert!(provider.inner.scopes.is_none());
        assert_eq!(provider.inner.refresh_buffer, Duration::from_secs(30));
    }

    #[test]
    fn test_builder_with_scopes() {
        let provider = OAuthClientCredentials::builder()
            .client_id("id")
            .client_secret("secret")
            .token_endpoint("https://auth.example.com/token")
            .scopes(["mcp:tools", "mcp:resources"])
            .build()
            .unwrap();

        assert_eq!(
            provider.inner.scopes.as_deref(),
            Some("mcp:tools mcp:resources")
        );
    }

    #[test]
    fn test_builder_with_refresh_buffer() {
        let provider = OAuthClientCredentials::builder()
            .client_id("id")
            .client_secret("secret")
            .token_endpoint("https://auth.example.com/token")
            .refresh_buffer(Duration::from_secs(60))
            .build()
            .unwrap();

        assert_eq!(provider.inner.refresh_buffer, Duration::from_secs(60));
    }

    #[test]
    fn test_debug_impl() {
        let provider = OAuthClientCredentials::builder()
            .client_id("my-client")
            .client_secret("secret")
            .token_endpoint("https://auth.example.com/token")
            .build()
            .unwrap();

        let debug = format!("{:?}", provider);
        assert!(debug.contains("my-client"));
        assert!(debug.contains("auth.example.com"));
        // Secret should NOT appear in debug output
        assert!(!debug.contains("secret"));
    }

    #[test]
    fn test_token_validity() {
        let valid_token = CachedToken {
            access_token: "valid".into(),
            expires_at: Instant::now() + Duration::from_secs(300),
        };
        assert!(OAuthClientCredentials::is_token_valid(
            &valid_token,
            Duration::from_secs(30)
        ));

        let expiring_soon = CachedToken {
            access_token: "expiring".into(),
            expires_at: Instant::now() + Duration::from_secs(10),
        };
        // 30s buffer > 10s remaining = invalid
        assert!(!OAuthClientCredentials::is_token_valid(
            &expiring_soon,
            Duration::from_secs(30)
        ));

        let expired = CachedToken {
            access_token: "expired".into(),
            expires_at: Instant::now() - Duration::from_secs(10),
        };
        assert!(!OAuthClientCredentials::is_token_valid(
            &expired,
            Duration::from_secs(30)
        ));
    }

    #[test]
    fn test_error_display() {
        let err = OAuthClientError::Discovery("not found".into());
        assert_eq!(err.to_string(), "OAuth discovery error: not found");

        let err = OAuthClientError::TokenRequest("timeout".into());
        assert_eq!(err.to_string(), "OAuth token request error: timeout");

        let err = OAuthClientError::InvalidResponse("bad json".into());
        assert_eq!(err.to_string(), "OAuth invalid response: bad json");

        let err = OAuthClientError::BuildError("missing field".into());
        assert_eq!(err.to_string(), "OAuth builder error: missing field");
    }

    #[tokio::test]
    async fn test_caching_returns_same_token() {
        // Manually insert a cached token and verify get_token returns it
        let provider = OAuthClientCredentials::builder()
            .client_id("id")
            .client_secret("secret")
            .token_endpoint("https://auth.example.com/token")
            .build()
            .unwrap();

        // Seed the cache directly
        {
            let mut cache = provider.inner.cache.write().await;
            *cache = Some(CachedToken {
                access_token: "cached-token-123".into(),
                expires_at: Instant::now() + Duration::from_secs(300),
            });
        }

        let token = provider.get_token().await.unwrap();
        assert_eq!(token, "cached-token-123");

        // Second call should return the same cached token
        let token2 = provider.get_token().await.unwrap();
        assert_eq!(token2, "cached-token-123");
    }

    #[tokio::test]
    async fn test_expired_token_triggers_refresh_attempt() {
        let provider = OAuthClientCredentials::builder()
            .client_id("id")
            .client_secret("secret")
            .token_endpoint("http://127.0.0.1:1/nonexistent")
            .build()
            .unwrap();

        // Seed with an expired token
        {
            let mut cache = provider.inner.cache.write().await;
            *cache = Some(CachedToken {
                access_token: "expired-token".into(),
                expires_at: Instant::now() - Duration::from_secs(60),
            });
        }

        // get_token should try to refresh and fail (unreachable endpoint)
        let err = provider.get_token().await.unwrap_err();
        assert!(matches!(err, OAuthClientError::TokenRequest(_)));
    }

    #[tokio::test]
    async fn test_custom_token_provider() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let count = call_count.clone();

        struct CountingProvider {
            count: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl TokenProvider for CountingProvider {
            async fn get_token(&self) -> Result<String, OAuthClientError> {
                let n = self.count.fetch_add(1, Ordering::SeqCst);
                Ok(format!("token-{}", n))
            }
        }

        let provider = CountingProvider { count };

        assert_eq!(provider.get_token().await.unwrap(), "token-0");
        assert_eq!(provider.get_token().await.unwrap(), "token-1");
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_clone() {
        let provider = OAuthClientCredentials::builder()
            .client_id("id")
            .client_secret("secret")
            .token_endpoint("https://auth.example.com/token")
            .build()
            .unwrap();

        let cloned = provider.clone();
        // Both share the same inner state
        assert!(Arc::ptr_eq(&provider.inner, &cloned.inner));
    }
}
