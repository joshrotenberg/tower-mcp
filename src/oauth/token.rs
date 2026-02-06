//! Token validation for OAuth 2.1 resource servers.
//!
//! Provides the [`TokenValidator`] trait for pluggable token validation,
//! [`JwtValidator`] for JWT validation with static keys, and (with the `jwks`
//! feature) [`JwksValidator`] for JWT validation using remotely fetched JWKS
//! endpoints.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;

use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};

use super::error::OAuthError;

/// Audience claim value, which can be a single string or array of strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TokenAudience {
    /// A single audience string.
    Single(String),
    /// Multiple audience strings.
    Multiple(Vec<String>),
}

impl TokenAudience {
    /// Check if the audience contains a specific value.
    pub fn contains(&self, value: &str) -> bool {
        match self {
            TokenAudience::Single(s) => s == value,
            TokenAudience::Multiple(v) => v.iter().any(|s| s == value),
        }
    }
}

/// Validated token claims extracted from an access token.
///
/// Contains standard JWT claims plus an `extra` map for custom claims.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    /// Subject (user/client identifier).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,

    /// Issuer URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,

    /// Audience (this resource server or other identifiers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aud: Option<TokenAudience>,

    /// Expiration time (Unix timestamp).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exp: Option<u64>,

    /// Space-delimited scope string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,

    /// OAuth client ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,

    /// Additional claims not covered by standard fields.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl TokenClaims {
    /// Parse the scope string into a set of individual scopes.
    pub fn scopes(&self) -> HashSet<String> {
        self.scope
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .map(String::from)
            .collect()
    }

    /// Check if the token has a specific scope.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes().contains(scope)
    }

    /// Check if the audience matches the given resource identifier.
    pub fn audience_matches(&self, resource: &str) -> bool {
        match &self.aud {
            Some(aud) => aud.contains(resource),
            None => true, // No audience claim means no restriction
        }
    }

    /// Check if the token has expired based on the current time.
    pub fn is_expired(&self) -> bool {
        match self.exp {
            Some(exp) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                now > exp
            }
            None => false, // No exp claim means no expiration
        }
    }
}

/// Trait for validating OAuth access tokens.
///
/// Implement this trait to provide custom token validation logic
/// (e.g., JWT verification, token introspection, opaque token lookup).
///
/// # Example
///
/// ```rust
/// use tower_mcp::oauth::{TokenValidator, TokenClaims, OAuthError};
///
/// #[derive(Clone)]
/// struct MyValidator;
///
/// impl TokenValidator for MyValidator {
///     async fn validate_token(&self, token: &str) -> Result<TokenClaims, OAuthError> {
///         // Custom validation logic here
///         # todo!()
///     }
/// }
/// ```
pub trait TokenValidator: Clone + Send + Sync + 'static {
    /// Validate an access token and return the extracted claims.
    ///
    /// Returns `Ok(TokenClaims)` if the token is valid, or an appropriate
    /// [`OAuthError`] if validation fails.
    fn validate_token(
        &self,
        token: &str,
    ) -> impl Future<Output = Result<TokenClaims, OAuthError>> + Send;
}

/// JWT token validator using static keys.
///
/// Validates JWTs using pre-configured decoding keys. Supports RSA, HMAC,
/// and EC algorithms via the `jsonwebtoken` crate.
///
/// # Example
///
/// ```rust
/// use tower_mcp::oauth::JwtValidator;
///
/// // HMAC-based validation
/// let validator = JwtValidator::from_secret(b"my-secret-key")
///     .expected_audience("https://mcp.example.com")
///     .expected_issuer("https://auth.example.com");
/// ```
#[derive(Clone)]
pub struct JwtValidator {
    decoding_key: Arc<DecodingKey>,
    validation: Arc<Validation>,
}

impl JwtValidator {
    /// Create a default `Validation` with audience validation disabled.
    ///
    /// By default, `jsonwebtoken::Validation` requires audience claims.
    /// We disable this by default and only enable it when
    /// [`expected_audience`](Self::expected_audience) is called.
    fn default_validation(algorithm: Algorithm) -> Validation {
        let mut validation = Validation::new(algorithm);
        // Disable audience validation by default -- callers opt in via expected_audience()
        validation.validate_aud = false;
        // Don't require any specific claims by default. The jsonwebtoken crate
        // requires "exp" by default, but OAuth tokens may omit it.
        validation.required_spec_claims.clear();
        validation
    }

    /// Create a validator from an HMAC secret.
    pub fn from_secret(secret: &[u8]) -> Self {
        let decoding_key = Arc::new(DecodingKey::from_secret(secret));
        let validation = Arc::new(Self::default_validation(Algorithm::HS256));
        Self {
            decoding_key,
            validation,
        }
    }

    /// Create a validator from an RSA PEM-encoded public key.
    ///
    /// # Errors
    ///
    /// Returns an error if the PEM data is invalid.
    pub fn from_rsa_pem(pem: &[u8]) -> Result<Self, jsonwebtoken::errors::Error> {
        let decoding_key = Arc::new(DecodingKey::from_rsa_pem(pem)?);
        let validation = Arc::new(Self::default_validation(Algorithm::RS256));
        Ok(Self {
            decoding_key,
            validation,
        })
    }

    /// Create a validator from an EC PEM-encoded public key.
    ///
    /// # Errors
    ///
    /// Returns an error if the PEM data is invalid.
    pub fn from_ec_pem(pem: &[u8]) -> Result<Self, jsonwebtoken::errors::Error> {
        let decoding_key = Arc::new(DecodingKey::from_ec_pem(pem)?);
        let validation = Arc::new(Self::default_validation(Algorithm::ES256));
        Ok(Self {
            decoding_key,
            validation,
        })
    }

    /// Set the expected audience for token validation.
    ///
    /// Tokens without a matching `aud` claim will be rejected.
    pub fn expected_audience(mut self, audience: &str) -> Self {
        let mut validation = (*self.validation).clone();
        validation.set_audience(&[audience]);
        self.validation = Arc::new(validation);
        self
    }

    /// Set the expected issuer for token validation.
    ///
    /// Tokens without a matching `iss` claim will be rejected.
    pub fn expected_issuer(mut self, issuer: &str) -> Self {
        let mut validation = (*self.validation).clone();
        validation.set_issuer(&[issuer]);
        self.validation = Arc::new(validation);
        self
    }

    /// Disable expiration validation.
    ///
    /// Use with caution -- tokens without expiration checks may be reused
    /// indefinitely.
    pub fn disable_exp_validation(mut self) -> Self {
        let mut validation = (*self.validation).clone();
        validation.validate_exp = false;
        self.validation = Arc::new(validation);
        self
    }

    /// Set the allowed algorithms for token validation.
    pub fn algorithms(mut self, algorithms: Vec<Algorithm>) -> Self {
        let mut validation = (*self.validation).clone();
        validation.algorithms = algorithms;
        self.validation = Arc::new(validation);
        self
    }
}

impl TokenValidator for JwtValidator {
    async fn validate_token(&self, token: &str) -> Result<TokenClaims, OAuthError> {
        let token_data =
            jsonwebtoken::decode::<TokenClaims>(token, &self.decoding_key, &self.validation)
                .map_err(|e| match e.kind() {
                    jsonwebtoken::errors::ErrorKind::ExpiredSignature => OAuthError::ExpiredToken,
                    jsonwebtoken::errors::ErrorKind::InvalidAudience => OAuthError::InvalidAudience,
                    _ => OAuthError::InvalidToken {
                        description: e.to_string(),
                    },
                })?;

        Ok(token_data.claims)
    }
}

/// Adapter that bridges the existing [`Validate`](crate::auth::Validate) trait
/// to [`TokenValidator`].
///
/// This allows reusing existing `Validate` implementations with the OAuth
/// middleware. The adapter creates minimal `TokenClaims` from a successful
/// validation.
///
/// # Example
///
/// ```rust
/// use tower_mcp::auth::StaticBearerValidator;
/// use tower_mcp::oauth::ValidateAdapter;
///
/// let bearer = StaticBearerValidator::new(vec!["token123".to_string()]);
/// let oauth_validator = ValidateAdapter::new(bearer);
/// ```
#[derive(Clone)]
pub struct ValidateAdapter<V> {
    inner: V,
}

impl<V> ValidateAdapter<V> {
    /// Create a new adapter wrapping an existing `Validate` implementation.
    pub fn new(inner: V) -> Self {
        Self { inner }
    }
}

impl<V: crate::auth::Validate> TokenValidator for ValidateAdapter<V> {
    async fn validate_token(&self, token: &str) -> Result<TokenClaims, OAuthError> {
        match self.inner.validate(token).await {
            crate::auth::AuthResult::Authenticated(info) => {
                let claims = TokenClaims {
                    sub: info.as_ref().map(|i| i.client_id.clone()),
                    iss: None,
                    aud: None,
                    exp: None,
                    scope: None,
                    client_id: info.as_ref().map(|i| i.client_id.clone()),
                    extra: info
                        .and_then(|i| i.claims)
                        .and_then(|v| serde_json::from_value(v).ok())
                        .unwrap_or_default(),
                };
                Ok(claims)
            }
            crate::auth::AuthResult::Failed(err) => Err(OAuthError::InvalidToken {
                description: err.message,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// JWKS-based token validation (feature = "jwks")
// ---------------------------------------------------------------------------

/// Errors from JWKS endpoint fetching and key parsing.
#[cfg(feature = "jwks")]
#[derive(Debug)]
pub enum JwksError {
    /// HTTP request to the JWKS endpoint failed.
    Fetch(reqwest::Error),
    /// The JWKS JSON could not be parsed.
    Parse(String),
    /// No suitable key was found in the JWKS for the given `kid`.
    KeyNotFound(Option<String>),
    /// A JWK could not be converted to a [`DecodingKey`].
    InvalidKey(String),
}

#[cfg(feature = "jwks")]
impl std::fmt::Display for JwksError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JwksError::Fetch(e) => write!(f, "JWKS fetch failed: {}", e),
            JwksError::Parse(msg) => write!(f, "JWKS parse error: {}", msg),
            JwksError::KeyNotFound(kid) => match kid {
                Some(kid) => write!(f, "no key found for kid \"{}\"", kid),
                None => write!(f, "no key found (no kid in token header)"),
            },
            JwksError::InvalidKey(msg) => write!(f, "invalid JWK: {}", msg),
        }
    }
}

#[cfg(feature = "jwks")]
impl std::error::Error for JwksError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            JwksError::Fetch(e) => Some(e),
            _ => None,
        }
    }
}

/// A cached JWKS decoding key with its algorithm.
#[cfg(feature = "jwks")]
#[derive(Clone)]
struct CachedKey {
    decoding_key: DecodingKey,
    algorithm: Algorithm,
}

/// The JWKS key cache.
#[cfg(feature = "jwks")]
struct JwksCache {
    /// Keys indexed by `kid`. Keys without a `kid` are stored under `None`.
    keys: HashMap<Option<String>, CachedKey>,
    /// When the cache was last populated.
    fetched_at: std::time::Instant,
    /// How long the cache is considered fresh.
    ttl: std::time::Duration,
}

#[cfg(feature = "jwks")]
impl JwksCache {
    fn is_expired(&self) -> bool {
        self.fetched_at.elapsed() > self.ttl
    }
}

/// Inner state shared via `Arc` for [`JwksValidator`].
#[cfg(feature = "jwks")]
struct JwksValidatorInner {
    jwks_url: String,
    client: reqwest::Client,
    cache: tokio::sync::RwLock<JwksCache>,
    validation_template: Validation,
    default_ttl: std::time::Duration,
    /// Minimum interval between fetches to prevent thundering herd.
    min_refresh_interval: std::time::Duration,
}

/// JWT validator that fetches keys from a remote JWKS endpoint.
///
/// Keys are cached and refreshed on-demand when expired or when a token
/// presents an unknown `kid`. The initial JWKS fetch happens eagerly during
/// [`JwksValidatorBuilder::build`] so configuration errors are caught early.
///
/// # Example
///
/// ```rust,no_run
/// use tower_mcp::oauth::JwksValidator;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let validator = JwksValidator::builder("https://auth.example.com/.well-known/jwks.json")
///     .expected_audience("https://mcp.example.com")
///     .expected_issuer("https://auth.example.com")
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
#[cfg(feature = "jwks")]
#[derive(Clone)]
pub struct JwksValidator {
    inner: Arc<JwksValidatorInner>,
}

/// Builder for [`JwksValidator`].
#[cfg(feature = "jwks")]
pub struct JwksValidatorBuilder {
    jwks_url: String,
    validation: Validation,
    default_ttl: std::time::Duration,
    min_refresh_interval: std::time::Duration,
    client: Option<reqwest::Client>,
}

#[cfg(feature = "jwks")]
impl JwksValidatorBuilder {
    /// Set the expected audience for token validation.
    ///
    /// Tokens without a matching `aud` claim will be rejected.
    pub fn expected_audience(mut self, audience: &str) -> Self {
        self.validation.set_audience(&[audience]);
        self.validation.validate_aud = true;
        self
    }

    /// Set the expected issuer for token validation.
    ///
    /// Tokens without a matching `iss` claim will be rejected.
    pub fn expected_issuer(mut self, issuer: &str) -> Self {
        self.validation.set_issuer(&[issuer]);
        self
    }

    /// Set the default cache TTL.
    ///
    /// This is used when the JWKS response does not include a `Cache-Control`
    /// header with a `max-age` directive. Defaults to 5 minutes.
    pub fn default_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.default_ttl = ttl;
        self
    }

    /// Set a custom `reqwest::Client` for JWKS fetching.
    ///
    /// Useful for configuring timeouts, proxies, or TLS settings.
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.client = Some(client);
        self
    }

    /// Disable expiration validation.
    ///
    /// Use with caution -- tokens without expiration checks may be reused
    /// indefinitely.
    pub fn disable_exp_validation(mut self) -> Self {
        self.validation.validate_exp = false;
        self
    }

    /// Build the validator, performing an initial JWKS fetch.
    ///
    /// # Errors
    ///
    /// Returns [`JwksError`] if the initial fetch or parse fails.
    pub async fn build(self) -> std::result::Result<JwksValidator, JwksError> {
        let client = self.client.unwrap_or_else(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("failed to build reqwest client")
        });

        let (keys, ttl) = fetch_and_parse_jwks(&client, &self.jwks_url, self.default_ttl).await?;

        let cache = JwksCache {
            keys,
            fetched_at: std::time::Instant::now(),
            ttl,
        };

        let inner = JwksValidatorInner {
            jwks_url: self.jwks_url,
            client,
            cache: tokio::sync::RwLock::new(cache),
            validation_template: self.validation,
            default_ttl: self.default_ttl,
            min_refresh_interval: self.min_refresh_interval,
        };

        Ok(JwksValidator {
            inner: Arc::new(inner),
        })
    }
}

#[cfg(feature = "jwks")]
impl JwksValidator {
    /// Create a new builder for a JWKS validator.
    ///
    /// `jwks_url` is the URL of the JWKS endpoint (e.g.,
    /// `https://auth.example.com/.well-known/jwks.json`).
    pub fn builder(jwks_url: impl Into<String>) -> JwksValidatorBuilder {
        let mut validation = Validation::default();
        // Allow all algorithms -- the per-key algorithm from JWKS will be used
        validation.algorithms = vec![
            Algorithm::RS256,
            Algorithm::RS384,
            Algorithm::RS512,
            Algorithm::ES256,
            Algorithm::ES384,
            Algorithm::PS256,
            Algorithm::PS384,
            Algorithm::PS512,
            Algorithm::EdDSA,
        ];
        validation.validate_aud = false;
        validation.required_spec_claims.clear();

        JwksValidatorBuilder {
            jwks_url: jwks_url.into(),
            validation,
            default_ttl: std::time::Duration::from_secs(300),
            min_refresh_interval: std::time::Duration::from_secs(10),
            client: None,
        }
    }

    /// Attempt to refresh the JWKS cache.
    ///
    /// Acquires a write lock, double-checks that enough time has elapsed since
    /// the last fetch (to avoid thundering herd), fetches, and updates the cache.
    /// On fetch failure, stale keys are retained.
    async fn refresh_cache(&self) -> std::result::Result<(), JwksError> {
        let mut cache = self.inner.cache.write().await;

        // Double-check: another task may have refreshed while we waited for the lock
        if cache.fetched_at.elapsed() < self.inner.min_refresh_interval {
            return Ok(());
        }

        match fetch_and_parse_jwks(
            &self.inner.client,
            &self.inner.jwks_url,
            self.inner.default_ttl,
        )
        .await
        {
            Ok((keys, ttl)) => {
                cache.keys = keys;
                cache.fetched_at = std::time::Instant::now();
                cache.ttl = ttl;
                Ok(())
            }
            Err(e) => {
                // On fetch failure, keep stale keys but update fetched_at to avoid
                // retrying immediately on every request
                tracing::warn!(error = %e, "JWKS refresh failed, using stale cache");
                if cache.keys.is_empty() {
                    // No stale keys to fall back on
                    Err(e)
                } else {
                    cache.fetched_at = std::time::Instant::now();
                    Ok(())
                }
            }
        }
    }

    /// Look up a key by `kid`, refreshing the cache if necessary.
    async fn get_key(&self, kid: Option<&str>) -> std::result::Result<CachedKey, JwksError> {
        // First try with a read lock
        {
            let cache = self.inner.cache.read().await;
            if !cache.is_expired()
                && let Some(key) = lookup_key(&cache.keys, kid)
            {
                return Ok(key);
            }
        }

        // Cache is expired or kid not found -- refresh
        self.refresh_cache().await?;

        // Retry lookup after refresh
        let cache = self.inner.cache.read().await;
        lookup_key(&cache.keys, kid).ok_or_else(|| JwksError::KeyNotFound(kid.map(String::from)))
    }
}

/// Look up a key by `kid` in the cache, with a single-key fallback.
#[cfg(feature = "jwks")]
fn lookup_key(keys: &HashMap<Option<String>, CachedKey>, kid: Option<&str>) -> Option<CachedKey> {
    // Try exact kid match
    if let Some(key) = keys.get(&kid.map(String::from)) {
        return Some(key.clone());
    }

    // Fallback: if there is exactly one key in the set, use it regardless of kid
    if keys.len() == 1 {
        return keys.values().next().cloned();
    }

    None
}

#[cfg(feature = "jwks")]
impl TokenValidator for JwksValidator {
    async fn validate_token(&self, token: &str) -> std::result::Result<TokenClaims, OAuthError> {
        // Decode the header to extract the kid
        let header = jsonwebtoken::decode_header(token).map_err(|e| OAuthError::InvalidToken {
            description: format!("failed to decode token header: {}", e),
        })?;

        let kid = header.kid.as_deref();

        let cached_key = self
            .get_key(kid)
            .await
            .map_err(|e| OAuthError::InvalidToken {
                description: e.to_string(),
            })?;

        // Build validation with the specific key's algorithm
        let mut validation = self.inner.validation_template.clone();
        validation.algorithms = vec![cached_key.algorithm];

        let token_data =
            jsonwebtoken::decode::<TokenClaims>(token, &cached_key.decoding_key, &validation)
                .map_err(|e| match e.kind() {
                    jsonwebtoken::errors::ErrorKind::ExpiredSignature => OAuthError::ExpiredToken,
                    jsonwebtoken::errors::ErrorKind::InvalidAudience => OAuthError::InvalidAudience,
                    _ => OAuthError::InvalidToken {
                        description: e.to_string(),
                    },
                })?;

        Ok(token_data.claims)
    }
}

/// Fetch a JWKS document and parse it into a key map.
///
/// Returns the key map and the TTL derived from the `Cache-Control` header
/// (or `default_ttl` if not present).
#[cfg(feature = "jwks")]
async fn fetch_and_parse_jwks(
    client: &reqwest::Client,
    url: &str,
    default_ttl: std::time::Duration,
) -> std::result::Result<(HashMap<Option<String>, CachedKey>, std::time::Duration), JwksError> {
    let response = client.get(url).send().await.map_err(JwksError::Fetch)?;

    let ttl = parse_cache_control_max_age(
        response
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
    )
    .unwrap_or(default_ttl);

    let jwks: jsonwebtoken::jwk::JwkSet = response
        .json()
        .await
        .map_err(|e| JwksError::Parse(e.to_string()))?;

    let keys = parse_jwk_set(&jwks)?;
    Ok((keys, ttl))
}

/// Parse a `Cache-Control` header value and extract the `max-age` directive.
#[cfg(feature = "jwks")]
fn parse_cache_control_max_age(header: Option<&str>) -> Option<std::time::Duration> {
    let header = header?;
    for directive in header.split(',') {
        let directive = directive.trim();
        if let Some(value) = directive.strip_prefix("max-age=")
            && let Ok(seconds) = value.trim().parse::<u64>()
        {
            return Some(std::time::Duration::from_secs(seconds));
        }
    }
    None
}

/// Convert a [`JwkSet`] into a map of `kid -> CachedKey`.
#[cfg(feature = "jwks")]
fn parse_jwk_set(
    jwks: &jsonwebtoken::jwk::JwkSet,
) -> std::result::Result<HashMap<Option<String>, CachedKey>, JwksError> {
    let mut keys = HashMap::new();

    for jwk in &jwks.keys {
        let algorithm = jwk
            .common
            .key_algorithm
            .and_then(key_algorithm_to_algorithm)
            .or_else(|| infer_algorithm_from_key_type(jwk))
            .ok_or_else(|| {
                JwksError::InvalidKey(format!(
                    "cannot determine algorithm for key {:?}",
                    jwk.common.key_id
                ))
            })?;

        let decoding_key = DecodingKey::from_jwk(jwk)
            .map_err(|e| JwksError::InvalidKey(format!("failed to create DecodingKey: {}", e)))?;

        let kid = jwk.common.key_id.clone();
        keys.insert(
            kid,
            CachedKey {
                decoding_key,
                algorithm,
            },
        );
    }

    Ok(keys)
}

/// Convert a `jsonwebtoken::jwk::KeyAlgorithm` to a `jsonwebtoken::Algorithm`.
#[cfg(feature = "jwks")]
fn key_algorithm_to_algorithm(ka: jsonwebtoken::jwk::KeyAlgorithm) -> Option<Algorithm> {
    use jsonwebtoken::jwk::KeyAlgorithm;
    match ka {
        KeyAlgorithm::RS256 => Some(Algorithm::RS256),
        KeyAlgorithm::RS384 => Some(Algorithm::RS384),
        KeyAlgorithm::RS512 => Some(Algorithm::RS512),
        KeyAlgorithm::ES256 => Some(Algorithm::ES256),
        KeyAlgorithm::ES384 => Some(Algorithm::ES384),
        KeyAlgorithm::PS256 => Some(Algorithm::PS256),
        KeyAlgorithm::PS384 => Some(Algorithm::PS384),
        KeyAlgorithm::PS512 => Some(Algorithm::PS512),
        KeyAlgorithm::EdDSA => Some(Algorithm::EdDSA),
        _ => None,
    }
}

/// Infer the algorithm from the JWK key type when `alg` is not specified.
#[cfg(feature = "jwks")]
fn infer_algorithm_from_key_type(jwk: &jsonwebtoken::jwk::Jwk) -> Option<Algorithm> {
    use jsonwebtoken::jwk::AlgorithmParameters;
    match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => Some(Algorithm::RS256),
        AlgorithmParameters::EllipticCurve(ec) => {
            use jsonwebtoken::jwk::EllipticCurve;
            match ec.curve {
                EllipticCurve::P256 => Some(Algorithm::ES256),
                EllipticCurve::P384 => Some(Algorithm::ES384),
                _ => None,
            }
        }
        AlgorithmParameters::OctetKeyPair(_) => Some(Algorithm::EdDSA),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_audience_single() {
        let aud = TokenAudience::Single("https://example.com".to_string());
        assert!(aud.contains("https://example.com"));
        assert!(!aud.contains("https://other.com"));
    }

    #[test]
    fn test_token_audience_multiple() {
        let aud = TokenAudience::Multiple(vec![
            "https://a.com".to_string(),
            "https://b.com".to_string(),
        ]);
        assert!(aud.contains("https://a.com"));
        assert!(aud.contains("https://b.com"));
        assert!(!aud.contains("https://c.com"));
    }

    #[test]
    fn test_token_claims_scopes() {
        let claims = TokenClaims {
            sub: None,
            iss: None,
            aud: None,
            exp: None,
            scope: Some("mcp:read mcp:write mcp:admin".to_string()),
            client_id: None,
            extra: HashMap::new(),
        };

        let scopes = claims.scopes();
        assert_eq!(scopes.len(), 3);
        assert!(claims.has_scope("mcp:read"));
        assert!(claims.has_scope("mcp:write"));
        assert!(claims.has_scope("mcp:admin"));
        assert!(!claims.has_scope("mcp:delete"));
    }

    #[test]
    fn test_token_claims_empty_scope() {
        let claims = TokenClaims {
            sub: None,
            iss: None,
            aud: None,
            exp: None,
            scope: None,
            client_id: None,
            extra: HashMap::new(),
        };

        assert!(claims.scopes().is_empty());
        assert!(!claims.has_scope("mcp:read"));
    }

    #[test]
    fn test_token_claims_audience_matches() {
        let claims = TokenClaims {
            sub: None,
            iss: None,
            aud: Some(TokenAudience::Single("https://mcp.example.com".to_string())),
            exp: None,
            scope: None,
            client_id: None,
            extra: HashMap::new(),
        };

        assert!(claims.audience_matches("https://mcp.example.com"));
        assert!(!claims.audience_matches("https://other.com"));
    }

    #[test]
    fn test_token_claims_no_audience() {
        let claims = TokenClaims {
            sub: None,
            iss: None,
            aud: None,
            exp: None,
            scope: None,
            client_id: None,
            extra: HashMap::new(),
        };

        // No audience claim means no restriction
        assert!(claims.audience_matches("anything"));
    }

    #[test]
    fn test_token_claims_expired() {
        let claims = TokenClaims {
            sub: None,
            iss: None,
            aud: None,
            exp: Some(0), // epoch = definitely expired
            scope: None,
            client_id: None,
            extra: HashMap::new(),
        };

        assert!(claims.is_expired());
    }

    #[test]
    fn test_token_claims_not_expired() {
        let future_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;

        let claims = TokenClaims {
            sub: None,
            iss: None,
            aud: None,
            exp: Some(future_time),
            scope: None,
            client_id: None,
            extra: HashMap::new(),
        };

        assert!(!claims.is_expired());
    }

    #[test]
    fn test_token_claims_no_exp() {
        let claims = TokenClaims {
            sub: None,
            iss: None,
            aud: None,
            exp: None,
            scope: None,
            client_id: None,
            extra: HashMap::new(),
        };

        assert!(!claims.is_expired());
    }

    #[tokio::test]
    async fn test_jwt_validator_hmac() {
        let secret = b"super-secret-key-for-testing-only";
        let validator = JwtValidator::from_secret(secret).disable_exp_validation();

        // Create a valid token
        let claims = serde_json::json!({
            "sub": "user123",
            "scope": "mcp:read mcp:write"
        });

        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(secret),
        )
        .unwrap();

        let result = validator.validate_token(&token).await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result.err());

        let validated = result.unwrap();
        assert_eq!(validated.sub.as_deref(), Some("user123"));
        assert!(validated.has_scope("mcp:read"));
        assert!(validated.has_scope("mcp:write"));
    }

    #[tokio::test]
    async fn test_jwt_validator_invalid_token() {
        let validator = JwtValidator::from_secret(b"secret");
        let result = validator.validate_token("not-a-jwt").await;
        assert!(result.is_err());
        assert!(matches!(result, Err(OAuthError::InvalidToken { .. })));
    }

    #[tokio::test]
    async fn test_jwt_validator_wrong_secret() {
        let secret = b"correct-secret";
        let wrong_secret = b"wrong-secret";

        let claims = serde_json::json!({"sub": "user"});
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(wrong_secret),
        )
        .unwrap();

        let validator = JwtValidator::from_secret(secret).disable_exp_validation();
        let result = validator.validate_token(&token).await;
        assert!(matches!(result, Err(OAuthError::InvalidToken { .. })));
    }

    #[tokio::test]
    async fn test_jwt_validator_expired_token() {
        let secret = b"secret";

        let claims = serde_json::json!({
            "sub": "user",
            "exp": 0  // epoch = expired
        });

        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(secret),
        )
        .unwrap();

        let validator = JwtValidator::from_secret(secret);
        let result = validator.validate_token(&token).await;
        assert!(matches!(result, Err(OAuthError::ExpiredToken)));
    }

    #[tokio::test]
    async fn test_validate_adapter() {
        use crate::auth::StaticBearerValidator;

        let bearer = StaticBearerValidator::new(vec!["valid-token".to_string()]);
        let adapter = ValidateAdapter::new(bearer);

        let result = adapter.validate_token("valid-token").await;
        assert!(result.is_ok());

        let claims = result.unwrap();
        assert!(claims.sub.is_some());

        let result = adapter.validate_token("invalid-token").await;
        assert!(matches!(result, Err(OAuthError::InvalidToken { .. })));
    }

    // JWKS-specific unit tests

    #[cfg(feature = "jwks")]
    #[test]
    fn test_parse_cache_control_max_age() {
        // Basic max-age
        assert_eq!(
            parse_cache_control_max_age(Some("max-age=3600")),
            Some(std::time::Duration::from_secs(3600))
        );

        // max-age with other directives
        assert_eq!(
            parse_cache_control_max_age(Some("public, max-age=600, must-revalidate")),
            Some(std::time::Duration::from_secs(600))
        );

        // No max-age
        assert_eq!(parse_cache_control_max_age(Some("no-cache")), None);

        // None header
        assert_eq!(parse_cache_control_max_age(None), None);

        // max-age=0
        assert_eq!(
            parse_cache_control_max_age(Some("max-age=0")),
            Some(std::time::Duration::from_secs(0))
        );
    }

    #[cfg(feature = "jwks")]
    #[test]
    fn test_parse_jwk_set_rsa() {
        // A minimal RSA JWK set
        let jwks_json = serde_json::json!({
            "keys": [{
                "kty": "RSA",
                "n": "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw",
                "e": "AQAB",
                "kid": "test-key-1",
                "alg": "RS256",
                "use": "sig"
            }]
        });

        let jwks: jsonwebtoken::jwk::JwkSet = serde_json::from_value(jwks_json).unwrap();
        let keys = parse_jwk_set(&jwks).unwrap();

        assert_eq!(keys.len(), 1);
        assert!(keys.contains_key(&Some("test-key-1".to_string())));

        let key = &keys[&Some("test-key-1".to_string())];
        assert!(matches!(key.algorithm, Algorithm::RS256));
    }

    #[cfg(feature = "jwks")]
    #[test]
    fn test_parse_jwk_set_no_kid() {
        let jwks_json = serde_json::json!({
            "keys": [{
                "kty": "RSA",
                "n": "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw",
                "e": "AQAB",
                "alg": "RS256",
                "use": "sig"
            }]
        });

        let jwks: jsonwebtoken::jwk::JwkSet = serde_json::from_value(jwks_json).unwrap();
        let keys = parse_jwk_set(&jwks).unwrap();

        assert_eq!(keys.len(), 1);
        // Key stored under None since no kid
        assert!(keys.contains_key(&None));
    }

    #[cfg(feature = "jwks")]
    #[test]
    fn test_key_algorithm_to_algorithm_mapping() {
        use jsonwebtoken::jwk::KeyAlgorithm;

        assert_eq!(
            key_algorithm_to_algorithm(KeyAlgorithm::RS256),
            Some(Algorithm::RS256)
        );
        assert_eq!(
            key_algorithm_to_algorithm(KeyAlgorithm::RS384),
            Some(Algorithm::RS384)
        );
        assert_eq!(
            key_algorithm_to_algorithm(KeyAlgorithm::RS512),
            Some(Algorithm::RS512)
        );
        assert_eq!(
            key_algorithm_to_algorithm(KeyAlgorithm::ES256),
            Some(Algorithm::ES256)
        );
        assert_eq!(
            key_algorithm_to_algorithm(KeyAlgorithm::ES384),
            Some(Algorithm::ES384)
        );
        assert_eq!(
            key_algorithm_to_algorithm(KeyAlgorithm::PS256),
            Some(Algorithm::PS256)
        );
        assert_eq!(
            key_algorithm_to_algorithm(KeyAlgorithm::PS384),
            Some(Algorithm::PS384)
        );
        assert_eq!(
            key_algorithm_to_algorithm(KeyAlgorithm::PS512),
            Some(Algorithm::PS512)
        );
        assert_eq!(
            key_algorithm_to_algorithm(KeyAlgorithm::EdDSA),
            Some(Algorithm::EdDSA)
        );
    }

    #[cfg(feature = "jwks")]
    #[test]
    fn test_infer_algorithm_from_rsa_key() {
        let jwk_json = serde_json::json!({
            "kty": "RSA",
            "n": "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw",
            "e": "AQAB",
            "use": "sig"
        });

        let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_value(jwk_json).unwrap();
        assert_eq!(infer_algorithm_from_key_type(&jwk), Some(Algorithm::RS256));
    }

    #[cfg(feature = "jwks")]
    #[test]
    fn test_lookup_key_exact_match() {
        let mut keys = HashMap::new();
        keys.insert(
            Some("key-1".to_string()),
            CachedKey {
                decoding_key: DecodingKey::from_secret(b"dummy"),
                algorithm: Algorithm::HS256,
            },
        );
        keys.insert(
            Some("key-2".to_string()),
            CachedKey {
                decoding_key: DecodingKey::from_secret(b"dummy2"),
                algorithm: Algorithm::HS384,
            },
        );

        let result = lookup_key(&keys, Some("key-1"));
        assert!(result.is_some());
        assert!(matches!(result.unwrap().algorithm, Algorithm::HS256));

        let result = lookup_key(&keys, Some("key-2"));
        assert!(result.is_some());
        assert!(matches!(result.unwrap().algorithm, Algorithm::HS384));

        // No match and multiple keys -> None
        let result = lookup_key(&keys, Some("key-3"));
        assert!(result.is_none());
    }

    #[cfg(feature = "jwks")]
    #[test]
    fn test_lookup_key_single_key_fallback() {
        let mut keys = HashMap::new();
        keys.insert(
            Some("key-1".to_string()),
            CachedKey {
                decoding_key: DecodingKey::from_secret(b"dummy"),
                algorithm: Algorithm::HS256,
            },
        );

        // Single key -> fallback regardless of kid
        let result = lookup_key(&keys, Some("different-kid"));
        assert!(result.is_some());

        let result = lookup_key(&keys, None);
        assert!(result.is_some());
    }

    #[cfg(feature = "jwks")]
    #[test]
    fn test_jwks_error_display() {
        let err = JwksError::KeyNotFound(Some("test-kid".to_string()));
        assert!(err.to_string().contains("test-kid"));

        let err = JwksError::KeyNotFound(None);
        assert!(err.to_string().contains("no kid"));

        let err = JwksError::Parse("bad json".to_string());
        assert!(err.to_string().contains("bad json"));

        let err = JwksError::InvalidKey("bad key".to_string());
        assert!(err.to_string().contains("bad key"));
    }
}
