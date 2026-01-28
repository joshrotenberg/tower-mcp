//! OAuth scope types and per-operation scope policy.
//!
//! Provides [`ScopeRequirement`] for defining required scopes and
//! [`ScopePolicy`] for mapping operations (tools, resources, prompts)
//! to their required scopes.

use std::collections::{HashMap, HashSet};

use super::error::OAuthError;
use super::token::TokenClaims;

/// A set of required OAuth scopes for an operation.
///
/// All scopes in the requirement must be present in the token for access
/// to be granted (AND semantics).
#[derive(Debug, Clone, Default)]
pub struct ScopeRequirement {
    required: HashSet<String>,
}

impl ScopeRequirement {
    /// Create an empty scope requirement (no scopes needed).
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a scope requirement from a single scope.
    pub fn one(scope: impl Into<String>) -> Self {
        let mut required = HashSet::new();
        required.insert(scope.into());
        Self { required }
    }

    /// Create a scope requirement from multiple scopes.
    pub fn all(scopes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            required: scopes.into_iter().map(Into::into).collect(),
        }
    }

    /// Add a required scope to this requirement.
    pub fn require(mut self, scope: impl Into<String>) -> Self {
        self.required.insert(scope.into());
        self
    }

    /// Check if the given token claims satisfy this requirement.
    ///
    /// Returns `Ok(())` if all required scopes are present, or
    /// `Err(OAuthError::InsufficientScope)` with details about
    /// which scopes are missing.
    pub fn check(&self, claims: &TokenClaims) -> Result<(), OAuthError> {
        if self.required.is_empty() {
            return Ok(());
        }

        let provided = claims.scopes();
        if self.required.is_subset(&provided) {
            Ok(())
        } else {
            Err(OAuthError::InsufficientScope {
                required: self.required.iter().cloned().collect(),
                provided: provided.into_iter().collect(),
            })
        }
    }

    /// Returns the required scopes.
    pub fn required_scopes(&self) -> &HashSet<String> {
        &self.required
    }

    /// Returns true if no scopes are required.
    pub fn is_empty(&self) -> bool {
        self.required.is_empty()
    }
}

/// Policy mapping MCP operations to their required OAuth scopes.
///
/// Allows configuring per-tool, per-resource, and per-prompt scope
/// requirements, with a default fallback.
///
/// # Example
///
/// ```rust
/// use tower_mcp::oauth::ScopePolicy;
///
/// let policy = ScopePolicy::new()
///     .default_scope("mcp:read")
///     .tool_scope("dangerous_tool", "mcp:admin")
///     .resource_scope("secret://data", "mcp:secret");
/// ```
#[derive(Debug, Clone, Default)]
pub struct ScopePolicy {
    default_scopes: ScopeRequirement,
    tool_scopes: HashMap<String, ScopeRequirement>,
    resource_scopes: HashMap<String, ScopeRequirement>,
    prompt_scopes: HashMap<String, ScopeRequirement>,
}

impl ScopePolicy {
    /// Create an empty scope policy (no scopes required for anything).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a default scope required for all operations.
    pub fn default_scope(mut self, scope: impl Into<String>) -> Self {
        self.default_scopes = self.default_scopes.require(scope);
        self
    }

    /// Set a default scope requirement for all operations.
    pub fn default_scopes(mut self, requirement: ScopeRequirement) -> Self {
        self.default_scopes = requirement;
        self
    }

    /// Set scope requirement for a specific tool.
    ///
    /// The tool scope is checked *in addition* to the default scope.
    pub fn tool_scope(mut self, tool_name: impl Into<String>, scope: impl Into<String>) -> Self {
        let name = tool_name.into();
        let entry = self.tool_scopes.entry(name).or_default();
        entry.required.insert(scope.into());
        self
    }

    /// Set scope requirement for a specific tool with a full requirement.
    pub fn tool_scopes(
        mut self,
        tool_name: impl Into<String>,
        requirement: ScopeRequirement,
    ) -> Self {
        self.tool_scopes.insert(tool_name.into(), requirement);
        self
    }

    /// Set scope requirement for a specific resource.
    pub fn resource_scope(
        mut self,
        resource_uri: impl Into<String>,
        scope: impl Into<String>,
    ) -> Self {
        let uri = resource_uri.into();
        let entry = self.resource_scopes.entry(uri).or_default();
        entry.required.insert(scope.into());
        self
    }

    /// Set scope requirement for a specific prompt.
    pub fn prompt_scope(
        mut self,
        prompt_name: impl Into<String>,
        scope: impl Into<String>,
    ) -> Self {
        let name = prompt_name.into();
        let entry = self.prompt_scopes.entry(name).or_default();
        entry.required.insert(scope.into());
        self
    }

    /// Check if the given claims satisfy the default scope requirement.
    pub fn check_default(&self, claims: &TokenClaims) -> Result<(), OAuthError> {
        self.default_scopes.check(claims)
    }

    /// Check if the given claims satisfy the scope requirement for a tool.
    ///
    /// Checks both default scopes and tool-specific scopes.
    pub fn check_tool(&self, tool_name: &str, claims: &TokenClaims) -> Result<(), OAuthError> {
        self.default_scopes.check(claims)?;
        if let Some(req) = self.tool_scopes.get(tool_name) {
            req.check(claims)?;
        }
        Ok(())
    }

    /// Check if the given claims satisfy the scope requirement for a resource.
    pub fn check_resource(
        &self,
        resource_uri: &str,
        claims: &TokenClaims,
    ) -> Result<(), OAuthError> {
        self.default_scopes.check(claims)?;
        if let Some(req) = self.resource_scopes.get(resource_uri) {
            req.check(claims)?;
        }
        Ok(())
    }

    /// Check if the given claims satisfy the scope requirement for a prompt.
    pub fn check_prompt(&self, prompt_name: &str, claims: &TokenClaims) -> Result<(), OAuthError> {
        self.default_scopes.check(claims)?;
        if let Some(req) = self.prompt_scopes.get(prompt_name) {
            req.check(claims)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn claims_with_scopes(scopes: &str) -> TokenClaims {
        TokenClaims {
            sub: Some("user".to_string()),
            iss: None,
            aud: None,
            exp: None,
            scope: Some(scopes.to_string()),
            client_id: None,
            extra: HashMap::new(),
        }
    }

    fn claims_no_scopes() -> TokenClaims {
        TokenClaims {
            sub: Some("user".to_string()),
            iss: None,
            aud: None,
            exp: None,
            scope: None,
            client_id: None,
            extra: HashMap::new(),
        }
    }

    #[test]
    fn test_scope_requirement_empty() {
        let req = ScopeRequirement::new();
        assert!(req.is_empty());
        assert!(req.check(&claims_no_scopes()).is_ok());
    }

    #[test]
    fn test_scope_requirement_one() {
        let req = ScopeRequirement::one("mcp:read");
        assert!(!req.is_empty());
        assert!(req.check(&claims_with_scopes("mcp:read mcp:write")).is_ok());
        assert!(req.check(&claims_no_scopes()).is_err());
    }

    #[test]
    fn test_scope_requirement_all() {
        let req = ScopeRequirement::all(["mcp:read", "mcp:write"]);
        assert!(req.check(&claims_with_scopes("mcp:read mcp:write")).is_ok());
        assert!(req.check(&claims_with_scopes("mcp:read")).is_err());
    }

    #[test]
    fn test_scope_requirement_insufficient() {
        let req = ScopeRequirement::one("mcp:admin");
        let result = req.check(&claims_with_scopes("mcp:read"));
        assert!(result.is_err());

        if let Err(OAuthError::InsufficientScope { required, provided }) = result {
            assert!(required.contains(&"mcp:admin".to_string()));
            assert!(provided.contains(&"mcp:read".to_string()));
        } else {
            panic!("Expected InsufficientScope error");
        }
    }

    #[test]
    fn test_scope_policy_default() {
        let policy = ScopePolicy::new().default_scope("mcp:read");

        assert!(
            policy
                .check_default(&claims_with_scopes("mcp:read"))
                .is_ok()
        );
        assert!(policy.check_default(&claims_no_scopes()).is_err());
    }

    #[test]
    fn test_scope_policy_tool_scope() {
        let policy = ScopePolicy::new()
            .default_scope("mcp:read")
            .tool_scope("dangerous", "mcp:admin");

        let read_user = claims_with_scopes("mcp:read");
        let admin_user = claims_with_scopes("mcp:read mcp:admin");

        // Default check passes for both
        assert!(policy.check_default(&read_user).is_ok());
        assert!(policy.check_default(&admin_user).is_ok());

        // Tool-specific check needs both default + tool scopes
        assert!(policy.check_tool("dangerous", &read_user).is_err());
        assert!(policy.check_tool("dangerous", &admin_user).is_ok());

        // Unknown tool only needs default scopes
        assert!(policy.check_tool("safe", &read_user).is_ok());
    }

    #[test]
    fn test_scope_policy_resource_scope() {
        let policy = ScopePolicy::new().resource_scope("secret://data", "mcp:secret");

        let user = claims_with_scopes("mcp:secret");
        let user_no_secret = claims_with_scopes("mcp:read");

        assert!(policy.check_resource("secret://data", &user).is_ok());
        assert!(
            policy
                .check_resource("secret://data", &user_no_secret)
                .is_err()
        );
        assert!(
            policy
                .check_resource("public://data", &user_no_secret)
                .is_ok()
        );
    }

    #[test]
    fn test_scope_policy_prompt_scope() {
        let policy = ScopePolicy::new().prompt_scope("admin-prompt", "mcp:admin");

        let admin = claims_with_scopes("mcp:admin");
        let user = claims_with_scopes("mcp:read");

        assert!(policy.check_prompt("admin-prompt", &admin).is_ok());
        assert!(policy.check_prompt("admin-prompt", &user).is_err());
        assert!(policy.check_prompt("public-prompt", &user).is_ok());
    }

    #[test]
    fn test_scope_policy_empty() {
        let policy = ScopePolicy::new();
        assert!(policy.check_default(&claims_no_scopes()).is_ok());
        assert!(policy.check_tool("any", &claims_no_scopes()).is_ok());
        assert!(
            policy
                .check_resource("any://uri", &claims_no_scopes())
                .is_ok()
        );
        assert!(policy.check_prompt("any", &claims_no_scopes()).is_ok());
    }
}
