//! Token validation for OAuth 2.1 resource servers.
//!
//! Provides the [`TokenValidator`] trait for pluggable token validation and
//! [`JwtValidator`] for JWT validation with static keys.

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
}
