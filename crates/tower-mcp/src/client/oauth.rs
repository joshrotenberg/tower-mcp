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
//!     .resource("https://mcp.example.com")
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
//!     "https://mcp.example.com",
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

use super::oauth_authcode::discover_oauth_authorization;

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

    /// Discard any cached token, so the next [`get_token`](Self::get_token)
    /// fetches a new one.
    ///
    /// Called when a server answers `401` with a Bearer challenge, which says
    /// the token was rejected regardless of what the cache believes about it.
    /// A token can stop working before it expires: it may be revoked, the
    /// authorization server's keys may rotate, or the two clocks may simply
    /// disagree. Without this the client would re-send the same rejected
    /// token and make no progress (#1370).
    ///
    /// The default does nothing, which keeps every existing implementation
    /// compiling and behaving as it did. A provider that caches should
    /// override it; one that mints a token per call already satisfies it.
    ///
    /// This may be called concurrently with `get_token`, and a caller has no
    /// way to know whether the token it is discarding is the one it was
    /// handed. Discarding a token another task just fetched costs one extra
    /// fetch, which is why this clears rather than takes a lock across the
    /// retry.
    async fn invalidate(&self) {}
}

/// Parameters from an OAuth Bearer `WWW-Authenticate` challenge.
///
/// MCP clients use the initial `401` challenge to locate Protected Resource
/// Metadata and select initial scopes. The same representation also covers an
/// `insufficient_scope` response received later in a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthBearerChallenge {
    /// OAuth error code, such as `invalid_token` or `insufficient_scope`.
    pub error: Option<String>,
    /// Space-delimited scopes supplied by the protected resource.
    pub scopes: Vec<String>,
    /// Protected Resource Metadata URL supplied by the server.
    pub resource_metadata: Option<String>,
    /// Human-readable description supplied by the server.
    pub error_description: Option<String>,
}

impl OAuthBearerChallenge {
    /// Parse the first Bearer challenge from a `WWW-Authenticate` header.
    ///
    /// The parser handles quoted commas and escaped quoted strings, and
    /// ignores other authentication schemes in a combined header value.
    pub fn from_www_authenticate(header: &str) -> Option<Self> {
        let mut in_bearer_challenge = false;
        let mut found_bearer_challenge = false;
        let mut error = None;
        let mut scope = None;
        let mut resource_metadata = None;
        let mut error_description = None;

        for segment in split_quoted_commas(header) {
            let segment = segment.trim();
            let (parameter, starts_challenge) = if let Some((candidate_scheme, remainder)) =
                segment.split_once(char::is_whitespace)
            {
                if !candidate_scheme.contains('=') {
                    if found_bearer_challenge {
                        break;
                    }
                    in_bearer_challenge = candidate_scheme.eq_ignore_ascii_case("Bearer");
                    found_bearer_challenge = in_bearer_challenge;
                    (remainder.trim(), true)
                } else {
                    (segment, false)
                }
            } else {
                (segment, false)
            };

            if starts_challenge && !in_bearer_challenge {
                continue;
            }
            if !in_bearer_challenge {
                continue;
            }

            let Some((name, value)) = parameter.split_once('=') else {
                continue;
            };
            let value = unquote_auth_param(value.trim())?;
            match name.trim() {
                name if name.eq_ignore_ascii_case("error") => error = Some(value),
                name if name.eq_ignore_ascii_case("scope") => scope = Some(value),
                name if name.eq_ignore_ascii_case("resource_metadata") => {
                    resource_metadata = Some(value)
                }
                name if name.eq_ignore_ascii_case("error_description") => {
                    error_description = Some(value)
                }
                _ => {}
            }
        }

        found_bearer_challenge.then(|| Self {
            error,
            scopes: unique_scopes(
                scope
                    .iter()
                    .flat_map(|value| value.split_ascii_whitespace()),
            ),
            resource_metadata,
            error_description,
        })
    }
}

/// An OAuth `insufficient_scope` Bearer challenge.
///
/// MCP authorization servers use this challenge on HTTP 403 responses to tell
/// clients which additional scopes are required for the attempted operation.
/// See [`OAuthScopeChallenge::from_www_authenticate`] for parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthScopeChallenge {
    /// Scopes requested by the authorization server.
    pub required_scopes: Vec<String>,
    /// Protected resource metadata URL supplied by the server.
    pub resource_metadata: Option<String>,
    /// Human-readable description supplied by the server.
    pub error_description: Option<String>,
}

impl OAuthScopeChallenge {
    /// Parse an `insufficient_scope` Bearer challenge from a
    /// `WWW-Authenticate` header value.
    ///
    /// Returns `None` for other authentication schemes, other OAuth errors,
    /// or challenges that do not include at least one required scope.
    pub fn from_www_authenticate(header: &str) -> Option<Self> {
        let challenge = OAuthBearerChallenge::from_www_authenticate(header)?;
        if challenge.error.as_deref() != Some("insufficient_scope") {
            return None;
        }
        if challenge.scopes.is_empty() {
            return None;
        }

        Some(Self {
            required_scopes: challenge.scopes,
            resource_metadata: challenge.resource_metadata,
            error_description: challenge.error_description,
        })
    }
}

/// Authentication method used at an OAuth token endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OAuthTokenEndpointAuthMethod {
    /// Public client authentication: send only `client_id` in the form body.
    None,
    /// HTTP Basic authentication using the client ID and secret.
    ClientSecretBasic,
    /// Client ID and secret in the form body.
    ClientSecretPost,
    /// JWT client assertion signed with the client's private key.
    PrivateKeyJwt,
}

impl OAuthTokenEndpointAuthMethod {
    pub(crate) fn select(
        advertised: &[String],
        has_client_secret: bool,
    ) -> Result<Self, OAuthClientError> {
        if advertised.is_empty() {
            return Ok(if has_client_secret {
                Self::ClientSecretBasic
            } else {
                Self::None
            });
        }

        if has_client_secret
            && advertised
                .iter()
                .any(|method| method == "client_secret_basic")
        {
            return Ok(Self::ClientSecretBasic);
        }
        if has_client_secret
            && advertised
                .iter()
                .any(|method| method == "client_secret_post")
        {
            return Ok(Self::ClientSecretPost);
        }
        if advertised.iter().any(|method| method == "none") {
            return Ok(Self::None);
        }

        Err(OAuthClientError::BuildError(format!(
            "authorization server supports no compatible token endpoint authentication method: {}",
            advertised.join(", ")
        )))
    }

    pub(crate) fn select_with_private_key(
        advertised: &[String],
        has_client_secret: bool,
        has_private_key_signer: bool,
    ) -> Result<Self, OAuthClientError> {
        if has_private_key_signer && advertised.iter().any(|method| method == "private_key_jwt") {
            return Ok(Self::PrivateKeyJwt);
        }
        Self::select(advertised, has_client_secret)
    }
}

/// Context passed to an [`OAuthScopeEscalationHandler`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthScopeEscalationRequest {
    /// MCP protected-resource URL that rejected the request.
    pub resource: String,
    /// MCP operation that was rejected, such as `tools/call:search`.
    pub operation: String,
    /// Parsed authorization-server challenge.
    pub challenge: OAuthScopeChallenge,
    /// Scopes held before this escalation.
    pub previous_scopes: Vec<String>,
    /// Stable-order union of previous and newly required scopes.
    pub requested_scopes: Vec<String>,
    /// One-based retry attempt for this resource operation.
    pub attempt: usize,
}

/// Application hook for interactive or headless OAuth reauthorization.
///
/// Implementations should acquire a token for `request.requested_scopes` and
/// update the [`TokenProvider`] installed with
/// [`HttpClientTransport::with_scope_aware_token_provider`](crate::client::HttpClientTransport::with_scope_aware_token_provider).
/// After this method succeeds, the transport asks that provider for a fresh
/// token and retries the rejected MCP operation.
#[async_trait]
pub trait OAuthScopeEscalationHandler: Send + Sync + 'static {
    /// Reauthorize for the stable-order scope union in `request`.
    async fn reauthorize(
        &self,
        request: OAuthScopeEscalationRequest,
    ) -> Result<(), OAuthClientError>;
}

/// Runtime scope-escalation policy for `HttpClientTransport`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthScopeEscalationConfig {
    initial_scopes: Vec<String>,
    max_attempts: usize,
}

impl Default for OAuthScopeEscalationConfig {
    fn default() -> Self {
        Self {
            initial_scopes: Vec::new(),
            max_attempts: 2,
        }
    }
}

impl OAuthScopeEscalationConfig {
    /// Create a policy with the scopes requested for the initial token.
    ///
    /// Duplicate and blank scope values are removed while preserving order.
    /// By default, at most two challenged retries are permitted per MCP
    /// operation.
    pub fn new(scopes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            initial_scopes: unique_scopes(scopes.into_iter().map(Into::into).flat_map(
                |scope: String| {
                    scope
                        .split_ascii_whitespace()
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                },
            )),
            ..Self::default()
        }
    }

    /// Set the maximum number of challenged retries for one MCP operation.
    ///
    /// Set this to zero to surface the first HTTP 403 without reauthorizing.
    pub fn max_attempts(mut self, max_attempts: usize) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    /// Initial scopes tracked by the policy.
    pub fn initial_scopes(&self) -> &[String] {
        &self.initial_scopes
    }

    /// Maximum challenged retries for one MCP operation.
    pub fn maximum_attempts(&self) -> usize {
        self.max_attempts
    }
}

fn unique_scopes(scopes: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<String> {
    let mut unique = Vec::new();
    for scope in scopes {
        let scope = scope.as_ref();
        if !scope.is_empty() && !unique.iter().any(|existing| existing == scope) {
            unique.push(scope.to_string());
        }
    }
    unique
}

fn split_quoted_commas(header: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in header.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                segments.push(&header[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    segments.push(&header[start..]);
    segments
}

fn unquote_auth_param(value: &str) -> Option<String> {
    if !value.starts_with('"') {
        return Some(value.to_string());
    }
    if !value.ends_with('"') || value.len() < 2 {
        return None;
    }

    let mut unquoted = String::new();
    let mut escaped = false;
    for character in value[1..value.len() - 1].chars() {
        if escaped {
            unquoted.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            unquoted.push(character);
        }
    }
    if escaped {
        return None;
    }
    Some(unquoted)
}

/// Error type for OAuth client operations.
#[derive(Debug)]
#[non_exhaustive]
pub enum OAuthClientError {
    /// OAuth protocol HTTP request failed.
    Http(String),
    /// Failed to discover the authorization server metadata.
    Discovery(String),
    /// Failed to request a token from the token endpoint.
    TokenRequest(String),
    /// Failed to register an OAuth client.
    Registration(String),
    /// Failed to load, save, or remove persisted OAuth client credentials.
    CredentialStore(String),
    /// Failed to load, save, or remove persisted OAuth tokens.
    TokenStore(String),
    /// Failed to load, save, or remove persisted PKCE/CSRF state.
    StateStore(String),
    /// Authorization redirect handling failed.
    Redirect(String),
    /// Failed to reauthorize after an insufficient-scope challenge.
    ScopeEscalation(String),
    /// The token response was invalid or missing required fields.
    InvalidResponse(String),
    /// Builder validation failed (missing required fields).
    BuildError(String),
}

impl fmt::Display for OAuthClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(msg) => write!(f, "OAuth HTTP error: {}", msg),
            Self::Discovery(msg) => write!(f, "OAuth discovery error: {}", msg),
            Self::TokenRequest(msg) => write!(f, "OAuth token request error: {}", msg),
            Self::Registration(msg) => write!(f, "OAuth client registration error: {}", msg),
            Self::CredentialStore(msg) => write!(f, "OAuth credential store error: {}", msg),
            Self::TokenStore(msg) => write!(f, "OAuth token store error: {}", msg),
            Self::StateStore(msg) => write!(f, "OAuth state store error: {}", msg),
            Self::Redirect(msg) => write!(f, "OAuth redirect error: {}", msg),
            Self::ScopeEscalation(msg) => write!(f, "OAuth scope escalation error: {}", msg),
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

/// Shared state for [`OAuthClientCredentials`].
struct OAuthClientCredentialsInner {
    client_id: String,
    client_secret: String,
    token_endpoint: String,
    token_endpoint_auth_method: OAuthTokenEndpointAuthMethod,
    resource: String,
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
///     .resource("https://mcp.example.com")
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
            .field(
                "token_endpoint_auth_method",
                &self.inner.token_endpoint_auth_method,
            )
            .field("resource", &self.inner.resource)
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

    /// Discover an MCP resource's authorization server and create a provider.
    ///
    /// This performs challenge-driven and path-aware Protected Resource
    /// Metadata discovery, validates authorization-server metadata, and binds
    /// token requests to the discovered resource using RFC 8707.
    ///
    /// # Errors
    ///
    /// Returns [`OAuthClientError::Discovery`] if the metadata endpoint
    /// is unreachable or does not contain a `token_endpoint`.
    pub async fn discover(
        resource_url: &str,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Result<Self, OAuthClientError> {
        let client = reqwest::Client::new();
        let discovery = discover_oauth_authorization(resource_url, None, &client).await?;
        let metadata = discovery.authorization_servers.first().ok_or_else(|| {
            OAuthClientError::Discovery("no authorization server discovered".into())
        })?;
        let auth_method = OAuthTokenEndpointAuthMethod::select(
            &metadata.token_endpoint_auth_methods_supported,
            true,
        )?;

        Self::builder()
            .client_id(client_id)
            .client_secret(client_secret)
            .token_endpoint(metadata.token_endpoint.clone())
            .token_endpoint_auth_method(auth_method)
            .resource(discovery.resource)
            .http_client(client)
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
        let mut params = vec![
            ("grant_type", "client_credentials".to_string()),
            ("resource", self.inner.resource.clone()),
        ];
        if let Some(ref scopes) = self.inner.scopes {
            params.push(("scope", scopes.clone()));
        }

        let mut request = self.inner.client.post(&self.inner.token_endpoint);
        match self.inner.token_endpoint_auth_method {
            OAuthTokenEndpointAuthMethod::None => {
                params.push(("client_id", self.inner.client_id.clone()));
            }
            OAuthTokenEndpointAuthMethod::ClientSecretBasic => {
                request =
                    request.basic_auth(&self.inner.client_id, Some(&self.inner.client_secret));
            }
            OAuthTokenEndpointAuthMethod::ClientSecretPost => {
                params.push(("client_id", self.inner.client_id.clone()));
                params.push(("client_secret", self.inner.client_secret.clone()));
            }
            OAuthTokenEndpointAuthMethod::PrivateKeyJwt => {
                return Err(OAuthClientError::BuildError(
                    "private_key_jwt requires OAuthAuthorizationFlow with a client assertion signer"
                        .to_string(),
                ));
            }
        }

        let response = request
            .form(&params)
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

    async fn invalidate(&self) {
        *self.inner.cache.write().await = None;
    }
}

/// Builder for [`OAuthClientCredentials`].
///
/// # Required Fields
///
/// - `client_id`
/// - `client_secret`
/// - `token_endpoint`
/// - `resource`
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
///     .resource("https://mcp.example.com")
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Default)]
pub struct OAuthClientCredentialsBuilder {
    client_id: Option<String>,
    client_secret: Option<String>,
    token_endpoint: Option<String>,
    token_endpoint_auth_method: Option<OAuthTokenEndpointAuthMethod>,
    resource: Option<String>,
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

    /// Set the token endpoint authentication method.
    ///
    /// Direct configurations default to `client_secret_basic`. Discovery
    /// selects a compatible method from authorization-server metadata.
    pub fn token_endpoint_auth_method(mut self, method: OAuthTokenEndpointAuthMethod) -> Self {
        self.token_endpoint_auth_method = Some(method);
        self
    }

    /// Set the canonical MCP protected-resource URI.
    ///
    /// The provider includes this value as the RFC 8707 `resource` parameter
    /// in every client-credentials token request.
    pub fn resource(mut self, resource: impl Into<String>) -> Self {
        self.resource = Some(resource.into());
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
    /// `token_endpoint`, or `resource` are not set.
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
        let resource = self
            .resource
            .ok_or_else(|| OAuthClientError::BuildError("resource is required".into()))?;

        let inner = OAuthClientCredentialsInner {
            client_id,
            client_secret,
            token_endpoint,
            token_endpoint_auth_method: self
                .token_endpoint_auth_method
                .unwrap_or(OAuthTokenEndpointAuthMethod::ClientSecretBasic),
            resource,
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
    fn test_builder_requires_resource_binding() {
        let err = OAuthClientCredentials::builder()
            .client_id("id")
            .client_secret("secret")
            .token_endpoint("https://auth.example.com/token")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("resource"));
    }

    #[test]
    fn test_builder_success() {
        let provider = OAuthClientCredentials::builder()
            .client_id("my-client")
            .client_secret("my-secret")
            .token_endpoint("https://auth.example.com/token")
            .resource("https://mcp.example.com")
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
            .resource("https://mcp.example.com")
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
            .resource("https://mcp.example.com")
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
            .resource("https://mcp.example.com")
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

        let err = OAuthClientError::Registration("rejected".into());
        assert_eq!(err.to_string(), "OAuth client registration error: rejected");

        let err = OAuthClientError::CredentialStore("unavailable".into());
        assert_eq!(err.to_string(), "OAuth credential store error: unavailable");

        let err = OAuthClientError::ScopeEscalation("denied".into());
        assert_eq!(err.to_string(), "OAuth scope escalation error: denied");

        let err = OAuthClientError::InvalidResponse("bad json".into());
        assert_eq!(err.to_string(), "OAuth invalid response: bad json");

        let err = OAuthClientError::BuildError("missing field".into());
        assert_eq!(err.to_string(), "OAuth builder error: missing field");
    }

    #[test]
    fn parses_insufficient_scope_challenge() {
        let challenge = OAuthScopeChallenge::from_www_authenticate(
            r#"Bearer error="insufficient_scope", scope="files.read files.write files.read", resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource", error_description="Need files, including \"shared\"""#,
        )
        .unwrap();

        assert_eq!(challenge.required_scopes, vec!["files.read", "files.write"]);
        assert_eq!(
            challenge.resource_metadata.as_deref(),
            Some("https://mcp.example.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(
            challenge.error_description.as_deref(),
            Some(r#"Need files, including "shared""#)
        );
    }

    #[test]
    fn parses_initial_bearer_discovery_challenge() {
        let challenge = OAuthBearerChallenge::from_www_authenticate(
            r#"Bearer resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource/mcp", scope="tools.read resources.read""#,
        )
        .unwrap();

        assert_eq!(challenge.error, None);
        assert_eq!(challenge.scopes, vec!["tools.read", "resources.read"]);
        assert_eq!(
            challenge.resource_metadata.as_deref(),
            Some("https://mcp.example.com/.well-known/oauth-protected-resource/mcp")
        );
    }

    #[test]
    fn token_auth_selection_follows_metadata_and_credentials() {
        assert_eq!(
            OAuthTokenEndpointAuthMethod::select(&[], true).unwrap(),
            OAuthTokenEndpointAuthMethod::ClientSecretBasic
        );
        assert_eq!(
            OAuthTokenEndpointAuthMethod::select(&["none".into()], false).unwrap(),
            OAuthTokenEndpointAuthMethod::None
        );
        assert_eq!(
            OAuthTokenEndpointAuthMethod::select(
                &["client_secret_post".into(), "client_secret_basic".into()],
                true,
            )
            .unwrap(),
            OAuthTokenEndpointAuthMethod::ClientSecretBasic
        );
        assert!(OAuthTokenEndpointAuthMethod::select(&["private_key_jwt".into()], true).is_err());
    }

    #[test]
    fn selects_bearer_from_multiple_authentication_challenges() {
        let challenge = OAuthScopeChallenge::from_www_authenticate(
            r#"Basic realm="legacy", Bearer realm="mcp", error="insufficient_scope", scope="tools.call""#,
        )
        .unwrap();

        assert_eq!(challenge.required_scopes, vec!["tools.call"]);
    }

    #[test]
    fn ignores_non_scope_authentication_challenges() {
        assert!(
            OAuthScopeChallenge::from_www_authenticate(
                r#"Bearer error="invalid_token", scope="tools.call""#
            )
            .is_none()
        );
        assert!(
            OAuthScopeChallenge::from_www_authenticate(
                r#"Bearer error="insufficient_scope", scope="""#
            )
            .is_none()
        );
        assert!(OAuthScopeChallenge::from_www_authenticate(r#"Basic realm="mcp""#).is_none());
    }

    #[test]
    fn scope_escalation_config_normalizes_scopes() {
        let config =
            OAuthScopeEscalationConfig::new(["openid profile", "profile", "", "tools.call"])
                .max_attempts(3);

        assert_eq!(
            config.initial_scopes(),
            &["openid", "profile", "tools.call"]
        );
        assert_eq!(config.maximum_attempts(), 3);
    }

    #[tokio::test]
    async fn test_caching_returns_same_token() {
        // Manually insert a cached token and verify get_token returns it
        let provider = OAuthClientCredentials::builder()
            .client_id("id")
            .client_secret("secret")
            .token_endpoint("https://auth.example.com/token")
            .resource("https://mcp.example.com")
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
            .resource("https://mcp.example.com")
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
    async fn client_credentials_posts_resource_and_selected_auth_method() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0_u8; 1024];
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&chunk[..read]);
                if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while bytes.len() < header_end + content_length {
                let mut chunk = [0_u8; 1024];
                let read = stream.read(&mut chunk).await.unwrap();
                bytes.extend_from_slice(&chunk[..read]);
            }
            let body = String::from_utf8_lossy(&bytes[header_end..header_end + content_length])
                .to_string();
            let response_body =
                r#"{"access_token":"service-token","token_type":"Bearer","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            body
        });

        let provider = OAuthClientCredentials::builder()
            .client_id("service-client")
            .client_secret("service-secret")
            .token_endpoint(endpoint)
            .token_endpoint_auth_method(OAuthTokenEndpointAuthMethod::ClientSecretPost)
            .resource("https://mcp.example.com/mcp")
            .scopes(["tools.call"])
            .build()
            .unwrap();
        assert_eq!(provider.get_token().await.unwrap(), "service-token");

        let body = server.await.unwrap();
        assert!(body.contains("grant_type=client_credentials"));
        assert!(body.contains("resource=https%3A%2F%2Fmcp.example.com%2Fmcp"));
        assert!(body.contains("client_id=service-client"));
        assert!(body.contains("client_secret=service-secret"));
        assert!(body.contains("scope=tools.call"));
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
            .resource("https://mcp.example.com")
            .build()
            .unwrap();

        let cloned = provider.clone();
        // Both share the same inner state
        assert!(Arc::ptr_eq(&provider.inner, &cloned.inner));
    }
}
