//! Protected Resource Metadata (RFC 9728 Section 3).
//!
//! Defines the metadata document served at `/.well-known/oauth-protected-resource`
//! to enable OAuth 2.1 client discovery of authorization servers.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

/// Errors returned when validating MCP Protected Resource Metadata.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProtectedResourceMetadataError {
    /// The resource identifier is not a valid absolute URL.
    #[error("invalid protected resource URL: {0}")]
    InvalidResourceUrl(#[source] url::ParseError),

    /// MCP HTTP resource identifiers must use HTTP or HTTPS.
    #[error("protected resource URL must use http or https, got {0}")]
    UnsupportedResourceScheme(String),

    /// Resource identifiers cannot contain URL fragments.
    #[error("protected resource URL must not contain a fragment")]
    ResourceHasFragment,

    /// MCP Protected Resource Metadata must advertise an authorization server.
    #[error("protected resource metadata must advertise at least one authorization server")]
    MissingAuthorizationServer,

    /// An advertised authorization-server issuer is not a valid absolute URL.
    #[error("invalid authorization server URL {url}: {source}")]
    InvalidAuthorizationServerUrl {
        /// The invalid issuer URL.
        url: String,
        /// The URL parsing failure.
        #[source]
        source: url::ParseError,
    },

    /// An advertised authorization-server issuer does not use HTTP(S).
    #[error("authorization server URL must use http or https, got {0}")]
    UnsupportedAuthorizationServerScheme(String),
}

/// Protected Resource Metadata per RFC 9728 Section 3.
///
/// This metadata document tells OAuth clients which authorization server(s)
/// to use and what scopes are available. It is served at
/// the RFC 9728 well-known location for the resource URL. For a resource with
/// a path, such as `https://example.com/mcp`, that location is
/// `https://example.com/.well-known/oauth-protected-resource/mcp`.
///
/// # Example
///
/// ```rust
/// use tower_mcp::oauth::ProtectedResourceMetadata;
///
/// let metadata = ProtectedResourceMetadata::new("https://mcp.example.com")
///     .authorization_server("https://auth.example.com")
///     .scope("mcp:read")
///     .scope("mcp:write")
///     .resource_documentation("https://docs.example.com/mcp");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtectedResourceMetadata {
    /// The resource server's identifier URL.
    ///
    /// This MUST be the URL the client uses to access the resource.
    pub resource: String,

    /// Authorization server issuer URLs that can issue tokens for this resource.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authorization_servers: Vec<String>,

    /// OAuth scopes supported by this resource server.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes_supported: Vec<String>,

    /// Methods supported for sending bearer tokens.
    ///
    /// Defaults to `["header"]` per RFC 6750.
    #[serde(default = "default_bearer_methods")]
    pub bearer_methods_supported: Vec<String>,

    /// URL of documentation for this resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_documentation: Option<String>,
}

fn default_bearer_methods() -> Vec<String> {
    vec!["header".to_string()]
}

impl ProtectedResourceMetadata {
    /// Create new metadata with the resource server's identifier URL.
    pub fn new(resource: impl Into<String>) -> Self {
        Self {
            resource: resource.into(),
            authorization_servers: Vec::new(),
            scopes_supported: Vec::new(),
            bearer_methods_supported: default_bearer_methods(),
            resource_documentation: None,
        }
    }

    /// Add an authorization server issuer URL.
    pub fn authorization_server(mut self, issuer_url: impl Into<String>) -> Self {
        self.authorization_servers.push(issuer_url.into());
        self
    }

    /// Add a supported OAuth scope.
    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.scopes_supported.push(scope.into());
        self
    }

    /// Set the resource documentation URL.
    pub fn resource_documentation(mut self, url: impl Into<String>) -> Self {
        self.resource_documentation = Some(url.into());
        self
    }

    /// Set the bearer methods supported.
    pub fn bearer_methods(mut self, methods: Vec<String>) -> Self {
        self.bearer_methods_supported = methods;
        self
    }

    /// Returns the well-known path for this metadata endpoint.
    ///
    /// This is the prefix used for root resources. For resources with a path,
    /// use [`Self::well_known_path_for_resource`].
    pub fn well_known_path() -> &'static str {
        "/.well-known/oauth-protected-resource"
    }

    /// Return the path-aware RFC 9728 well-known path for a resource URL.
    ///
    /// A resource at `https://example.com/mcp` maps to
    /// `/.well-known/oauth-protected-resource/mcp`. Query and fragment
    /// components are not copied to the metadata endpoint.
    pub fn well_known_path_for_resource(
        resource: &str,
    ) -> Result<String, ProtectedResourceMetadataError> {
        let url = Self::parse_resource_url(resource)?;
        let resource_path = url.path();
        if resource_path.is_empty() || resource_path == "/" {
            Ok(Self::well_known_path().to_string())
        } else {
            Ok(format!("{}{}", Self::well_known_path(), resource_path))
        }
    }

    /// Return the absolute RFC 9728 metadata URL for this resource.
    pub fn well_known_url(&self) -> Result<String, ProtectedResourceMetadataError> {
        let url = Self::parse_resource_url(&self.resource)?;
        let path = Self::well_known_path_for_resource(&self.resource)?;
        Ok(format!("{}{}", url.origin().ascii_serialization(), path))
    }

    /// Validate metadata required by an MCP OAuth resource server.
    ///
    /// This checks that the resource is an absolute HTTP(S) URL without a
    /// fragment and that at least one valid authorization-server URL is
    /// advertised.
    pub fn validate(&self) -> Result<(), ProtectedResourceMetadataError> {
        Self::parse_resource_url(&self.resource)?;
        if self.authorization_servers.is_empty() {
            return Err(ProtectedResourceMetadataError::MissingAuthorizationServer);
        }
        for issuer in &self.authorization_servers {
            let url = Url::parse(issuer).map_err(|source| {
                ProtectedResourceMetadataError::InvalidAuthorizationServerUrl {
                    url: issuer.clone(),
                    source,
                }
            })?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(
                    ProtectedResourceMetadataError::UnsupportedAuthorizationServerScheme(
                        url.scheme().to_string(),
                    ),
                );
            }
        }
        Ok(())
    }

    fn parse_resource_url(resource: &str) -> Result<Url, ProtectedResourceMetadataError> {
        let url =
            Url::parse(resource).map_err(ProtectedResourceMetadataError::InvalidResourceUrl)?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ProtectedResourceMetadataError::UnsupportedResourceScheme(
                url.scheme().to_string(),
            ));
        }
        if url.fragment().is_some() {
            return Err(ProtectedResourceMetadataError::ResourceHasFragment);
        }
        Ok(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder() {
        let metadata = ProtectedResourceMetadata::new("https://mcp.example.com")
            .authorization_server("https://auth.example.com")
            .scope("mcp:read")
            .scope("mcp:write")
            .resource_documentation("https://docs.example.com");

        assert_eq!(metadata.resource, "https://mcp.example.com");
        assert_eq!(
            metadata.authorization_servers,
            vec!["https://auth.example.com"]
        );
        assert_eq!(metadata.scopes_supported, vec!["mcp:read", "mcp:write"]);
        assert_eq!(metadata.bearer_methods_supported, vec!["header"]);
        assert_eq!(
            metadata.resource_documentation.as_deref(),
            Some("https://docs.example.com")
        );
    }

    #[test]
    fn test_serialization() {
        let metadata = ProtectedResourceMetadata::new("https://mcp.example.com")
            .authorization_server("https://auth.example.com")
            .scope("mcp:read");

        let json = serde_json::to_value(&metadata).unwrap();
        assert_eq!(json["resource"], "https://mcp.example.com");
        assert_eq!(json["authorization_servers"][0], "https://auth.example.com");
        assert_eq!(json["scopes_supported"][0], "mcp:read");
        assert_eq!(json["bearer_methods_supported"][0], "header");
        // resource_documentation should be absent (None)
        assert!(json.get("resource_documentation").is_none());
    }

    #[test]
    fn test_deserialization() {
        let json = serde_json::json!({
            "resource": "https://mcp.example.com",
            "authorization_servers": ["https://auth.example.com"],
            "scopes_supported": ["mcp:read"],
            "bearer_methods_supported": ["header"]
        });

        let metadata: ProtectedResourceMetadata = serde_json::from_value(json).unwrap();
        assert_eq!(metadata.resource, "https://mcp.example.com");
        assert_eq!(metadata.authorization_servers.len(), 1);
        assert_eq!(metadata.scopes_supported.len(), 1);
    }

    #[test]
    fn test_well_known_path() {
        assert_eq!(
            ProtectedResourceMetadata::well_known_path(),
            "/.well-known/oauth-protected-resource"
        );
    }

    #[test]
    fn test_multiple_auth_servers() {
        let metadata = ProtectedResourceMetadata::new("https://mcp.example.com")
            .authorization_server("https://auth1.example.com")
            .authorization_server("https://auth2.example.com");

        assert_eq!(metadata.authorization_servers.len(), 2);
    }

    #[test]
    fn test_path_aware_well_known_location() {
        let metadata = ProtectedResourceMetadata::new("https://mcp.example.com/tenant/mcp?x=1")
            .authorization_server("https://auth.example.com");

        assert_eq!(
            metadata.well_known_url().unwrap(),
            "https://mcp.example.com/.well-known/oauth-protected-resource/tenant/mcp"
        );
        assert_eq!(
            ProtectedResourceMetadata::well_known_path_for_resource(&metadata.resource).unwrap(),
            "/.well-known/oauth-protected-resource/tenant/mcp"
        );
    }

    #[test]
    fn test_path_aware_location_preserves_encoded_path_segments() {
        let metadata = ProtectedResourceMetadata::new("https://mcp.example.com/a%2Fb")
            .authorization_server("https://auth.example.com");
        assert_eq!(
            metadata.well_known_url().unwrap(),
            "https://mcp.example.com/.well-known/oauth-protected-resource/a%2Fb"
        );
    }

    #[test]
    fn test_validate_requires_authorization_server() {
        let error = ProtectedResourceMetadata::new("https://mcp.example.com")
            .validate()
            .unwrap_err();
        assert!(matches!(
            error,
            ProtectedResourceMetadataError::MissingAuthorizationServer
        ));
    }

    #[test]
    fn test_validate_rejects_resource_fragment() {
        let error = ProtectedResourceMetadata::new("https://mcp.example.com#fragment")
            .authorization_server("https://auth.example.com")
            .validate()
            .unwrap_err();
        assert!(matches!(
            error,
            ProtectedResourceMetadataError::ResourceHasFragment
        ));
    }
}
