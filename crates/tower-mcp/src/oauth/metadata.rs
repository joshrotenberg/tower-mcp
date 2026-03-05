//! Protected Resource Metadata (RFC 9728 Section 3).
//!
//! Defines the metadata document served at `/.well-known/oauth-protected-resource`
//! to enable OAuth 2.1 client discovery of authorization servers.

use serde::{Deserialize, Serialize};

/// Protected Resource Metadata per RFC 9728 Section 3.
///
/// This metadata document tells OAuth clients which authorization server(s)
/// to use and what scopes are available. It is served at
/// `/.well-known/oauth-protected-resource` relative to the resource's base URL.
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
    /// Per RFC 9728, the metadata is served at
    /// `/.well-known/oauth-protected-resource` relative to the resource URL.
    pub fn well_known_path() -> &'static str {
        "/.well-known/oauth-protected-resource"
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
}
