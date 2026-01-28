//! OAuth 2.1 error types and WWW-Authenticate header construction.
//!
//! Implements error responses per RFC 6750 Section 3, including the
//! `resource_metadata` parameter from RFC 9728 for Protected Resource
//! Metadata discovery.

use std::fmt;

/// OAuth 2.1 authentication/authorization error.
///
/// Each variant maps to a specific HTTP status code and `WWW-Authenticate`
/// header value per RFC 6750 Section 3.
#[derive(Debug, Clone)]
pub enum OAuthError {
    /// No bearer token was provided in the request.
    /// Returns HTTP 401 with `WWW-Authenticate: Bearer`.
    MissingToken,

    /// The provided token is invalid (malformed, signature mismatch, etc.).
    /// Returns HTTP 401 with `error="invalid_token"`.
    InvalidToken {
        /// Human-readable description of why the token is invalid.
        description: String,
    },

    /// The token's scopes are insufficient for the requested operation.
    /// Returns HTTP 403 with `error="insufficient_scope"`.
    InsufficientScope {
        /// Scopes required by the operation.
        required: Vec<String>,
        /// Scopes present in the token.
        provided: Vec<String>,
    },

    /// The token's audience does not match this resource server.
    /// Returns HTTP 401 with `error="invalid_token"`.
    InvalidAudience,

    /// The token has expired.
    /// Returns HTTP 401 with `error="invalid_token"`.
    ExpiredToken,
}

impl OAuthError {
    /// Returns the HTTP status code for this error.
    ///
    /// - 401 Unauthorized for authentication failures (missing/invalid/expired token)
    /// - 403 Forbidden for authorization failures (insufficient scope)
    pub fn status_code(&self) -> u16 {
        match self {
            OAuthError::InsufficientScope { .. } => 403,
            _ => 401,
        }
    }

    /// Builds the `WWW-Authenticate` header value per RFC 6750 Section 3.
    ///
    /// When `resource_metadata_url` is provided, includes the `resource_metadata`
    /// parameter per RFC 9728 so clients can discover the authorization server.
    pub fn www_authenticate(&self, resource_metadata_url: Option<&str>) -> String {
        let mut parts = Vec::new();

        // Add resource_metadata parameter if available
        if let Some(url) = resource_metadata_url {
            parts.push(format!("resource_metadata=\"{}\"", url));
        }

        match self {
            OAuthError::MissingToken => {
                // RFC 6750 Section 3: If the request lacks any authentication
                // information, the resource server SHOULD NOT include an error code.
                if parts.is_empty() {
                    return "Bearer".to_string();
                }
                format!("Bearer {}", parts.join(", "))
            }
            OAuthError::InvalidToken { description } => {
                parts.push("error=\"invalid_token\"".to_string());
                parts.push(format!("error_description=\"{}\"", description));
                format!("Bearer {}", parts.join(", "))
            }
            OAuthError::InsufficientScope { required, .. } => {
                parts.push("error=\"insufficient_scope\"".to_string());
                if !required.is_empty() {
                    parts.push(format!("scope=\"{}\"", required.join(" ")));
                }
                format!("Bearer {}", parts.join(", "))
            }
            OAuthError::InvalidAudience => {
                parts.push("error=\"invalid_token\"".to_string());
                parts.push(
                    "error_description=\"The token audience does not match this resource\""
                        .to_string(),
                );
                format!("Bearer {}", parts.join(", "))
            }
            OAuthError::ExpiredToken => {
                parts.push("error=\"invalid_token\"".to_string());
                parts.push("error_description=\"The access token has expired\"".to_string());
                format!("Bearer {}", parts.join(", "))
            }
        }
    }
}

impl fmt::Display for OAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OAuthError::MissingToken => write!(f, "missing bearer token"),
            OAuthError::InvalidToken { description } => {
                write!(f, "invalid token: {}", description)
            }
            OAuthError::InsufficientScope { required, provided } => write!(
                f,
                "insufficient scope: required [{}], provided [{}]",
                required.join(", "),
                provided.join(", ")
            ),
            OAuthError::InvalidAudience => write!(f, "token audience does not match"),
            OAuthError::ExpiredToken => write!(f, "token has expired"),
        }
    }
}

impl std::error::Error for OAuthError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_missing_token_no_metadata() {
        let err = OAuthError::MissingToken;
        assert_eq!(err.status_code(), 401);
        assert_eq!(err.www_authenticate(None), "Bearer");
    }

    #[test]
    fn test_missing_token_with_metadata() {
        let err = OAuthError::MissingToken;
        assert_eq!(err.status_code(), 401);
        let header = err.www_authenticate(Some(
            "https://example.com/.well-known/oauth-protected-resource",
        ));
        assert!(header.starts_with("Bearer "));
        assert!(header.contains("resource_metadata="));
    }

    #[test]
    fn test_invalid_token() {
        let err = OAuthError::InvalidToken {
            description: "signature mismatch".to_string(),
        };
        assert_eq!(err.status_code(), 401);
        let header = err.www_authenticate(None);
        assert!(header.contains("error=\"invalid_token\""));
        assert!(header.contains("error_description=\"signature mismatch\""));
    }

    #[test]
    fn test_insufficient_scope() {
        let err = OAuthError::InsufficientScope {
            required: vec!["mcp:admin".to_string()],
            provided: vec!["mcp:read".to_string()],
        };
        assert_eq!(err.status_code(), 403);
        let header = err.www_authenticate(None);
        assert!(header.contains("error=\"insufficient_scope\""));
        assert!(header.contains("scope=\"mcp:admin\""));
    }

    #[test]
    fn test_invalid_audience() {
        let err = OAuthError::InvalidAudience;
        assert_eq!(err.status_code(), 401);
        let header = err.www_authenticate(None);
        assert!(header.contains("error=\"invalid_token\""));
        assert!(header.contains("audience"));
    }

    #[test]
    fn test_expired_token() {
        let err = OAuthError::ExpiredToken;
        assert_eq!(err.status_code(), 401);
        let header = err.www_authenticate(None);
        assert!(header.contains("error=\"invalid_token\""));
        assert!(header.contains("expired"));
    }

    #[test]
    fn test_display() {
        assert_eq!(OAuthError::MissingToken.to_string(), "missing bearer token");
        assert_eq!(OAuthError::ExpiredToken.to_string(), "token has expired");
        assert_eq!(
            OAuthError::InvalidAudience.to_string(),
            "token audience does not match"
        );
    }
}
