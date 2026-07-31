//! OAuth scope types and per-operation scope policy.
//!
//! Provides [`ScopeRequirement`] for defining required scopes and
//! [`ScopePolicy`] for mapping operations (tools, resources, prompts)
//! to their required scopes.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use super::error::OAuthError;
use super::token::TokenClaims;

/// Determines whether a granted OAuth scope satisfies a required scope.
///
/// The default matcher uses exact string equality. Implement this trait (or
/// pass a closure to [`ScopePolicy::scope_matcher`]) when an authorization
/// server defines hierarchical scopes such as `mcp:*` implying `mcp:read`.
pub trait ScopeMatcher: Send + Sync + 'static {
    /// Return `true` when `granted` authorizes an operation requiring
    /// `required`.
    fn matches(&self, granted: &str, required: &str) -> bool;
}

impl<F> ScopeMatcher for F
where
    F: Fn(&str, &str) -> bool + Send + Sync + 'static,
{
    fn matches(&self, granted: &str, required: &str) -> bool {
        self(granted, required)
    }
}

#[derive(Debug, Default)]
struct ExactScopeMatcher;

impl ScopeMatcher for ExactScopeMatcher {
    fn matches(&self, granted: &str, required: &str) -> bool {
        granted == required
    }
}

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
        self.check_with(claims, &ExactScopeMatcher)
    }

    /// Check the requirement using a custom scope matcher.
    pub fn check_with(
        &self,
        claims: &TokenClaims,
        matcher: &dyn ScopeMatcher,
    ) -> Result<(), OAuthError> {
        if self.required.is_empty() {
            return Ok(());
        }

        let provided = claims.scopes();
        let satisfied = self.required.iter().all(|required| {
            provided
                .iter()
                .any(|granted| matcher.matches(granted, required))
        });
        if satisfied {
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
#[derive(Clone)]
pub struct ScopePolicy {
    default_scopes: ScopeRequirement,
    tool_scopes: HashMap<String, ScopeRequirement>,
    resource_scopes: HashMap<String, ScopeRequirement>,
    prompt_scopes: HashMap<String, ScopeRequirement>,
    matcher: Arc<dyn ScopeMatcher>,
}

impl Default for ScopePolicy {
    fn default() -> Self {
        Self {
            default_scopes: ScopeRequirement::default(),
            tool_scopes: HashMap::new(),
            resource_scopes: HashMap::new(),
            prompt_scopes: HashMap::new(),
            matcher: Arc::new(ExactScopeMatcher),
        }
    }
}

impl fmt::Debug for ScopePolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScopePolicy")
            .field("default_scopes", &self.default_scopes)
            .field("tool_scopes", &self.tool_scopes)
            .field("resource_scopes", &self.resource_scopes)
            .field("prompt_scopes", &self.prompt_scopes)
            .field("matcher", &"<scope matcher>")
            .finish()
    }
}

impl ScopePolicy {
    /// Create an empty scope policy (no scopes required for anything).
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a custom matcher for exact or hierarchical scope semantics.
    ///
    /// The matcher receives `(granted, required)` and should return `true`
    /// when the granted scope authorizes the required scope.
    pub fn scope_matcher(mut self, matcher: impl ScopeMatcher) -> Self {
        self.matcher = Arc::new(matcher);
        self
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
        self.default_scopes
            .check_with(claims, self.matcher.as_ref())
    }

    /// Check if the given claims satisfy the scope requirement for a tool.
    ///
    /// Checks both default scopes and tool-specific scopes.
    pub fn check_tool(&self, tool_name: &str, claims: &TokenClaims) -> Result<(), OAuthError> {
        self.default_scopes
            .check_with(claims, self.matcher.as_ref())?;
        if let Some(req) = self.tool_scopes.get(tool_name) {
            req.check_with(claims, self.matcher.as_ref())?;
        }
        Ok(())
    }

    /// Check if the given claims satisfy the scope requirement for a resource.
    pub fn check_resource(
        &self,
        resource_uri: &str,
        claims: &TokenClaims,
    ) -> Result<(), OAuthError> {
        self.default_scopes
            .check_with(claims, self.matcher.as_ref())?;
        if let Some(req) = self.resource_scopes.get(resource_uri) {
            req.check_with(claims, self.matcher.as_ref())?;
        }
        Ok(())
    }

    /// Check if the given claims satisfy the scope requirement for a prompt.
    pub fn check_prompt(&self, prompt_name: &str, claims: &TokenClaims) -> Result<(), OAuthError> {
        self.default_scopes
            .check_with(claims, self.matcher.as_ref())?;
        if let Some(req) = self.prompt_scopes.get(prompt_name) {
            req.check_with(claims, self.matcher.as_ref())?;
        }
        Ok(())
    }
}

// =============================================================================
// ScopeEnforcementLayer -- tower middleware at the MCP RouterRequest level
// =============================================================================

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tower::Layer;
use tower_service::Service;

use crate::error::JsonRpcError;
use crate::protocol::McpRequest;
use crate::router::{RouterRequest, RouterResponse};

/// Tower layer that enforces OAuth scope requirements at the MCP request level.
///
/// Unlike [`OAuthLayer`](super::OAuthLayer) which operates at the HTTP transport
/// level, `ScopeEnforcementLayer` operates on [`RouterRequest`] and can perform
/// per-operation scope checks (e.g., different scopes for different tools).
///
/// The middleware extracts [`TokenClaims`] from [`RouterRequest::extensions`]. If
/// no claims are present, the request is rejected. Use
/// [`ScopeEnforcementLayer::permissive_without_claims`] only when a surrounding
/// component intentionally supports unauthenticated requests.
///
/// # Example
///
/// ```rust,no_run
/// use std::time::Duration;
/// use tower_mcp::McpRouter;
/// use tower_mcp::oauth::{ScopePolicy, ScopeEnforcementLayer};
/// use tower_mcp::transport::http::HttpTransport;
///
/// let policy = ScopePolicy::new()
///     .default_scope("mcp:read")
///     .tool_scope("admin_tool", "mcp:admin");
///
/// let router = McpRouter::new().server_info("my-server", "1.0.0");
/// let transport = HttpTransport::new(router)
///     .layer(ScopeEnforcementLayer::new(policy));
/// ```
#[derive(Debug, Clone)]
pub struct ScopeEnforcementLayer {
    policy: ScopePolicy,
    require_claims: bool,
}

impl ScopeEnforcementLayer {
    /// Create a new scope enforcement layer with the given policy.
    pub fn new(policy: ScopePolicy) -> Self {
        Self {
            policy,
            require_claims: true,
        }
    }

    /// Create a layer that skips scope checks when authentication claims are absent.
    ///
    /// This is an explicit opt-out from fail-closed behavior. Prefer [`Self::new`]
    /// for protected MCP endpoints.
    pub fn permissive_without_claims(policy: ScopePolicy) -> Self {
        Self {
            policy,
            require_claims: false,
        }
    }
}

impl<S> Layer<S> for ScopeEnforcementLayer {
    type Service = ScopeEnforcementService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ScopeEnforcementService {
            inner,
            policy: self.policy.clone(),
            require_claims: self.require_claims,
        }
    }
}

/// Tower service that enforces OAuth scope requirements on MCP requests.
///
/// Created by [`ScopeEnforcementLayer`]. For each incoming `RouterRequest`:
///
/// 1. Extracts [`TokenClaims`] from `req.extensions`
/// 2. If no claims are present, rejects the request unless permissive mode was requested
/// 3. Matches the request type (`CallTool`, `ReadResource`, `GetPrompt`, etc.)
/// 4. Checks the appropriate scope requirement from the [`ScopePolicy`]
/// 5. On failure, returns a `RouterResponse` with a JSON-RPC forbidden error
/// 6. On success, forwards to the inner service
#[derive(Debug, Clone)]
pub struct ScopeEnforcementService<S> {
    inner: S,
    policy: ScopePolicy,
    require_claims: bool,
}

impl<S> Service<RouterRequest> for ScopeEnforcementService<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        // Extract claims from extensions; fail closed unless explicitly configured otherwise.
        let claims = req.extensions.get::<TokenClaims>().cloned();

        let Some(claims) = claims else {
            if self.require_claims {
                let response = RouterResponse {
                    id: req.id,
                    inner: Err(JsonRpcError::forbidden(
                        "authenticated token claims are required",
                    )),
                };
                return Box::pin(async move { Ok(response) });
            }
            return Box::pin(self.inner.call(req));
        };

        // Check scope based on request type
        let check_result = match &req.inner {
            McpRequest::CallTool(params) => self.policy.check_tool(&params.name, &claims),
            McpRequest::ReadResource(params) => self.policy.check_resource(&params.uri, &claims),
            McpRequest::GetPrompt(params) => self.policy.check_prompt(&params.name, &claims),
            // All other request types use the default scope check
            _ => self.policy.check_default(&claims),
        };

        if let Err(err) = check_result {
            let response = RouterResponse {
                id: req.id,
                inner: Err(JsonRpcError::forbidden(err.to_string())),
            };
            return Box::pin(async move { Ok(response) });
        }

        let fut = self.inner.call(req);
        Box::pin(fut)
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

    #[test]
    fn test_scope_policy_custom_hierarchy() {
        let policy = ScopePolicy::new()
            .default_scope("mcp:read")
            .tool_scope("admin", "mcp:admin")
            .scope_matcher(|granted: &str, required: &str| {
                granted == required || granted == "mcp:*"
            });
        let claims = claims_with_scopes("mcp:*");

        assert!(policy.check_default(&claims).is_ok());
        assert!(policy.check_tool("admin", &claims).is_ok());
    }

    #[test]
    fn test_scope_enforcement_is_fail_closed_by_default() {
        assert!(ScopeEnforcementLayer::new(ScopePolicy::new()).require_claims);
        assert!(
            !ScopeEnforcementLayer::permissive_without_claims(ScopePolicy::new()).require_claims
        );
    }
}
