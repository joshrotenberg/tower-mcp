//! Session-based capability filtering.
//!
//! This module provides types for filtering tools, resources, and prompts
//! based on session state. Different sessions can see different capabilities
//! based on user identity, roles, API keys, or other session context.
//!
//! # Example
//!
//! ```rust
//! use tower_mcp::{McpRouter, ToolBuilder, CallToolResult, CapabilityFilter, Tool, Filterable};
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(Debug, Deserialize, JsonSchema)]
//! struct Input { value: String }
//!
//! let public_tool = ToolBuilder::new("public")
//!     .description("Available to everyone")
//!     .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
//!     .build();
//!
//! let admin_tool = ToolBuilder::new("admin")
//!     .description("Admin only")
//!     .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
//!     .build();
//!
//! let router = McpRouter::new()
//!     .tool(public_tool)
//!     .tool(admin_tool)
//!     .tool_filter(CapabilityFilter::new(|_session, tool: &Tool| {
//!         // In real code, check session.extensions() for auth claims
//!         tool.name() != "admin"
//!     }));
//! ```

use std::collections::HashSet;
use std::sync::Arc;

use crate::context::Extensions;
use crate::error::{Error, JsonRpcError};
use crate::prompt::Prompt;
use crate::resource::{Resource, ResourceTemplate};
use crate::session::SessionState;
use crate::tool::Tool;

/// Trait for capabilities that can be filtered by session.
///
/// Implemented for [`Tool`], [`Resource`], [`ResourceTemplate`], and [`Prompt`].
pub trait Filterable: Send + Sync {
    /// Returns the name of this capability.
    fn name(&self) -> &str;
}

impl Filterable for Tool {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Filterable for Resource {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Filterable for ResourceTemplate {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Filterable for Prompt {
    fn name(&self) -> &str {
        &self.name
    }
}

/// Behavior when a filtered capability is accessed directly.
#[derive(Clone, Default)]
#[non_exhaustive]
pub enum DenialBehavior {
    /// Return "method not found" error -- hides the capability entirely.
    ///
    /// This is the default and recommended for security. Use this in
    /// multi-tenant scenarios where tools should not be discoverable by
    /// unauthorized users.
    #[default]
    NotFound,
    /// Return an "unauthorized" error, revealing the capability exists.
    ///
    /// Use this when the client should know about the capability but is
    /// not permitted to invoke it (e.g., premium features behind an
    /// upgrade prompt).
    Unauthorized,
    /// Use a custom error generator for application-specific responses.
    ///
    /// Use this when you need custom status codes, domain-specific error
    /// messages, or structured error payloads.
    Custom(Arc<dyn Fn(&str) -> Error + Send + Sync>),
}

impl std::fmt::Debug for DenialBehavior {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "NotFound"),
            Self::Unauthorized => write!(f, "Unauthorized"),
            Self::Custom(_) => write!(f, "Custom(...)"),
        }
    }
}

impl DenialBehavior {
    /// Create a custom denial behavior with the given error generator.
    pub fn custom<F>(f: F) -> Self
    where
        F: Fn(&str) -> Error + Send + Sync + 'static,
    {
        Self::Custom(Arc::new(f))
    }

    /// Generate the appropriate error for a denied capability.
    pub fn to_error(&self, name: &str) -> Error {
        match self {
            Self::NotFound => Error::JsonRpc(JsonRpcError::method_not_found(name)),
            Self::Unauthorized => {
                Error::JsonRpc(JsonRpcError::forbidden(format!("Unauthorized: {}", name)))
            }
            Self::Custom(f) => f(name),
        }
    }
}

/// The operation for which a capability policy is being evaluated.
///
/// List operations decide whether a definition may be disclosed. Access
/// operations authorize a concrete target before its handler is invoked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CapabilityOperation<'a> {
    /// The capability is being considered for a list or generated catalog.
    List,
    /// The capability is being accessed directly.
    Access {
        /// The concrete client-supplied target, such as a tool name or
        /// resolved resource URI.
        target: &'a str,
    },
}

/// Request-aware context supplied to contextual capability filters.
///
/// This is intentionally lighter than [`crate::RequestContext`]: evaluating a
/// visibility policy must not register cancellation or in-flight request
/// state. Router-level extensions are merged with per-request extensions, and
/// per-request values win when the same type is present in both maps.
pub struct CapabilityFilterContext<'a> {
    session: &'a SessionState,
    extensions: Extensions,
    operation: CapabilityOperation<'a>,
}

impl<'a> CapabilityFilterContext<'a> {
    pub(crate) fn new(
        session: &'a SessionState,
        router_extensions: &Extensions,
        request_extensions: &Extensions,
        operation: CapabilityOperation<'a>,
    ) -> Self {
        let mut extensions = router_extensions.clone();
        extensions.merge(request_extensions);
        Self {
            session,
            extensions,
            operation,
        }
    }

    /// Return the logical MCP session being evaluated.
    pub fn session(&self) -> &SessionState {
        self.session
    }

    /// Return a typed router- or request-level extension.
    ///
    /// If both scopes contain the same type, the per-request value is
    /// returned.
    pub fn extension<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.extensions.get::<T>()
    }

    /// Return all merged extensions visible to the policy.
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    /// Return the operation being authorized.
    pub fn operation(&self) -> CapabilityOperation<'a> {
        self.operation
    }

    /// Return the concrete target for an access operation.
    pub fn target(&self) -> Option<&'a str> {
        match self.operation {
            CapabilityOperation::List => None,
            CapabilityOperation::Access { target } => Some(target),
        }
    }
}

/// A filter for capabilities based on session state.
///
/// Use this to control which tools, resources, or prompts are visible
/// to each session.
///
/// # Example
///
/// ```rust
/// use tower_mcp::{CapabilityFilter, DenialBehavior, Tool, Filterable};
///
/// // Filter that only shows tools starting with "public_"
/// let filter = CapabilityFilter::new(|_session, tool: &Tool| {
///     tool.name().starts_with("public_")
/// });
///
/// // Filter with custom denial behavior
/// let filter_with_401 = CapabilityFilter::new(|_session, tool: &Tool| {
///     tool.name() != "admin"
/// }).denial_behavior(DenialBehavior::Unauthorized);
/// ```
pub struct CapabilityFilter<T: Filterable> {
    #[allow(clippy::type_complexity)]
    filter: Arc<dyn for<'a> Fn(&CapabilityFilterContext<'a>, &T) -> bool + Send + Sync>,
    denial: DenialBehavior,
}

impl<T: Filterable> Clone for CapabilityFilter<T> {
    fn clone(&self) -> Self {
        Self {
            filter: Arc::clone(&self.filter),
            denial: self.denial.clone(),
        }
    }
}

impl<T: Filterable> std::fmt::Debug for CapabilityFilter<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityFilter")
            .field("denial", &self.denial)
            .finish_non_exhaustive()
    }
}

impl<T: Filterable> CapabilityFilter<T> {
    /// Create a new capability filter with the given predicate.
    ///
    /// The predicate receives the session state and capability, and returns
    /// `true` if the capability should be visible to the session.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{CapabilityFilter, Tool, Filterable};
    ///
    /// let filter = CapabilityFilter::new(|_session, tool: &Tool| {
    ///     // Check session extensions for auth claims
    ///     // session.extensions().get::<UserClaims>()...
    ///     tool.name() != "admin_only"
    /// });
    /// ```
    pub fn new<F>(filter: F) -> Self
    where
        F: Fn(&SessionState, &T) -> bool + Send + Sync + 'static,
    {
        Self::new_with_context(move |context, capability| filter(context.session(), capability))
    }

    /// Create a request-aware capability filter.
    ///
    /// The policy can inspect the logical session, router or per-request
    /// extensions, and whether it is evaluating catalog disclosure or direct
    /// access. For resource templates, an access operation's target is the
    /// concrete resolved URI rather than the template pattern.
    pub fn new_with_context<F>(filter: F) -> Self
    where
        F: for<'a> Fn(&CapabilityFilterContext<'a>, &T) -> bool + Send + Sync + 'static,
    {
        Self {
            filter: Arc::new(filter),
            denial: DenialBehavior::default(),
        }
    }

    /// Set the behavior when a filtered capability is accessed directly.
    ///
    /// Default is [`DenialBehavior::NotFound`].
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{CapabilityFilter, DenialBehavior, Tool, Filterable};
    ///
    /// let filter = CapabilityFilter::new(|_, tool: &Tool| tool.name() != "secret")
    ///     .denial_behavior(DenialBehavior::Unauthorized);
    /// ```
    pub fn denial_behavior(mut self, behavior: DenialBehavior) -> Self {
        self.denial = behavior;
        self
    }

    /// Check if the given capability is visible to the session.
    pub fn is_visible(&self, session: &SessionState, capability: &T) -> bool {
        let empty = Extensions::new();
        let context =
            CapabilityFilterContext::new(session, &empty, &empty, CapabilityOperation::List);
        self.is_visible_with_context(&context, capability)
    }

    /// Check whether the capability is visible for a request-aware operation.
    pub fn is_visible_with_context(
        &self,
        context: &CapabilityFilterContext<'_>,
        capability: &T,
    ) -> bool {
        (self.filter)(context, capability)
    }

    /// Get the error to return when access is denied.
    pub fn denial_error(&self, name: &str) -> Error {
        self.denial.to_error(name)
    }

    /// Create a filter that only shows capabilities whose names are in the list.
    ///
    /// Capabilities not in the list are hidden. This is useful for exposing
    /// a curated subset of capabilities (e.g., from a config file or CLI flag).
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{CapabilityFilter, Tool};
    ///
    /// // Only expose these two tools
    /// let filter = CapabilityFilter::<Tool>::allow_list(&["query", "list_tables"]);
    /// ```
    pub fn allow_list(names: &[&str]) -> Self
    where
        T: 'static,
    {
        let allowed: HashSet<String> = names.iter().map(|s| (*s).to_string()).collect();
        Self::new(move |_session, cap: &T| allowed.contains(cap.name()))
    }

    /// Create a filter that hides capabilities whose names are in the list.
    ///
    /// All capabilities are visible except those explicitly listed. This is
    /// useful for blocking specific dangerous or irrelevant capabilities.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{CapabilityFilter, Tool};
    ///
    /// // Hide these destructive tools
    /// let filter = CapabilityFilter::<Tool>::deny_list(&["delete", "drop_table"]);
    /// ```
    pub fn deny_list(names: &[&str]) -> Self
    where
        T: 'static,
    {
        let denied: HashSet<String> = names.iter().map(|s| (*s).to_string()).collect();
        Self::new(move |_session, cap: &T| !denied.contains(cap.name()))
    }
}

impl CapabilityFilter<Tool> {
    /// Create a filter that blocks non-read-only tools when the predicate returns `false`.
    ///
    /// Read-only tools (those with `read_only_hint = true`) are always allowed.
    /// Non-read-only tools are only allowed when `is_write_allowed` returns `true`
    /// for the current session.
    ///
    /// This provides annotation-based write protection without requiring
    /// manual guards in every write tool handler.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{CapabilityFilter, Tool};
    ///
    /// // Block all write tools unconditionally
    /// let filter = CapabilityFilter::<Tool>::write_guard(|_session| false);
    ///
    /// // Allow writes based on session state
    /// // let filter = CapabilityFilter::<Tool>::write_guard(|session| {
    /// //     session.get::<WriteEnabled>().is_some()
    /// // });
    /// ```
    pub fn write_guard<F>(is_write_allowed: F) -> Self
    where
        F: Fn(&SessionState) -> bool + Send + Sync + 'static,
    {
        Self::new(move |session, tool: &Tool| {
            let read_only = tool.annotations.as_ref().is_some_and(|a| a.read_only_hint);
            read_only || is_write_allowed(session)
        })
    }
}

/// Type alias for tool filters.
pub type ToolFilter = CapabilityFilter<Tool>;

/// Type alias for resource filters.
pub type ResourceFilter = CapabilityFilter<Resource>;

/// Type alias for resource template filters.
pub type ResourceTemplateFilter = CapabilityFilter<ResourceTemplate>;

/// Type alias for prompt filters.
pub type PromptFilter = CapabilityFilter<Prompt>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CallToolResult;
    use crate::protocol::ReadResourceResult;
    use crate::resource::ResourceTemplateBuilder;
    use crate::tool::ToolBuilder;

    fn make_test_tool(name: &str) -> Tool {
        ToolBuilder::new(name)
            .description("Test tool")
            .handler(|_: serde_json::Value| async { Ok(CallToolResult::text("ok")) })
            .build()
    }

    fn make_test_template(name: &str) -> ResourceTemplate {
        ResourceTemplateBuilder::new(format!("test://{name}/{{id}}"))
            .name(name)
            .handler(|uri, _variables| async move { Ok(ReadResourceResult::text(uri, "ok")) })
    }

    #[test]
    fn test_filter_allows() {
        let filter = CapabilityFilter::new(|_, tool: &Tool| tool.name() != "blocked");
        let session = SessionState::new();
        let allowed = make_test_tool("allowed");
        let blocked = make_test_tool("blocked");

        assert!(filter.is_visible(&session, &allowed));
        assert!(!filter.is_visible(&session, &blocked));
    }

    #[test]
    fn contextual_filter_sees_session_operation_and_request_extension_precedence() {
        #[derive(Clone, Debug, PartialEq, Eq)]
        struct Role(&'static str);
        #[derive(Debug, PartialEq, Eq)]
        struct Identity(&'static str);

        let session = SessionState::new();
        session.insert(Role("admin"));
        let mut router_extensions = Extensions::new();
        router_extensions.insert(Identity("router"));
        let mut request_extensions = Extensions::new();
        request_extensions.insert(Identity("request"));
        let tool = make_test_tool("inspect");

        let filter = CapabilityFilter::new_with_context(
            |context: &CapabilityFilterContext<'_>, tool: &Tool| {
                context.session().get::<Role>() == Some(Role("admin"))
                    && context.extension::<Identity>() == Some(&Identity("request"))
                    && context.extensions().contains::<Identity>()
                    && context.operation() == (CapabilityOperation::Access { target: "inspect" })
                    && context.target() == Some("inspect")
                    && tool.name() == "inspect"
            },
        );
        let context = CapabilityFilterContext::new(
            &session,
            &router_extensions,
            &request_extensions,
            CapabilityOperation::Access { target: "inspect" },
        );

        assert!(filter.is_visible_with_context(&context, &tool));
    }

    #[test]
    fn resource_template_filter_uses_the_template_name() {
        let session = SessionState::new();
        let filter = ResourceTemplateFilter::allow_list(&["public"]);

        assert!(filter.is_visible(&session, &make_test_template("public")));
        assert!(!filter.is_visible(&session, &make_test_template("private")));
    }

    #[test]
    fn test_denial_behavior_not_found() {
        let behavior = DenialBehavior::NotFound;
        let error = behavior.to_error("test_tool");
        assert!(matches!(error, Error::JsonRpc(_)));
    }

    #[test]
    fn test_denial_behavior_unauthorized() {
        let behavior = DenialBehavior::Unauthorized;
        let error = behavior.to_error("test_tool");
        match error {
            Error::JsonRpc(e) => {
                assert_eq!(e.code, -32007); // McpErrorCode::Forbidden
                assert!(e.message.contains("Unauthorized"));
            }
            _ => panic!("Expected JsonRpc error"),
        }
    }

    #[test]
    fn test_denial_behavior_custom() {
        let behavior = DenialBehavior::custom(|name| Error::tool(format!("No access to {}", name)));
        let error = behavior.to_error("secret_tool");
        match error {
            Error::Tool(e) => {
                assert!(e.message.contains("No access to secret_tool"));
            }
            _ => panic!("Expected Tool error"),
        }
    }

    #[test]
    fn test_filter_clone() {
        let filter = CapabilityFilter::new(|_, _: &Tool| true);
        let cloned = filter.clone();
        let session = SessionState::new();
        let tool = make_test_tool("test");
        assert!(cloned.is_visible(&session, &tool));
    }

    #[test]
    fn test_filter_with_denial_behavior() {
        let filter = CapabilityFilter::new(|_, _: &Tool| false)
            .denial_behavior(DenialBehavior::Unauthorized);

        let error = filter.denial_error("test");
        match error {
            Error::JsonRpc(e) => assert_eq!(e.code, -32007), // McpErrorCode::Forbidden
            _ => panic!("Expected JsonRpc error"),
        }
    }

    fn make_read_only_tool(name: &str) -> Tool {
        ToolBuilder::new(name)
            .description("Read-only tool")
            .read_only()
            .handler(|_: serde_json::Value| async { Ok(CallToolResult::text("ok")) })
            .build()
    }

    #[test]
    fn test_write_guard_allows_read_only_when_writes_blocked() {
        let filter = CapabilityFilter::<Tool>::write_guard(|_| false);
        let session = SessionState::new();
        let tool = make_read_only_tool("reader");

        assert!(filter.is_visible(&session, &tool));
    }

    #[test]
    fn test_write_guard_blocks_write_tool_when_writes_blocked() {
        let filter = CapabilityFilter::<Tool>::write_guard(|_| false);
        let session = SessionState::new();
        let tool = make_test_tool("writer");

        assert!(!filter.is_visible(&session, &tool));
    }

    #[test]
    fn test_write_guard_allows_write_tool_when_writes_allowed() {
        let filter = CapabilityFilter::<Tool>::write_guard(|_| true);
        let session = SessionState::new();
        let tool = make_test_tool("writer");

        assert!(filter.is_visible(&session, &tool));
    }

    #[test]
    fn test_write_guard_with_denial_behavior() {
        let filter = CapabilityFilter::<Tool>::write_guard(|_| false)
            .denial_behavior(DenialBehavior::Unauthorized);
        let session = SessionState::new();
        let tool = make_test_tool("writer");

        assert!(!filter.is_visible(&session, &tool));
        let error = filter.denial_error("writer");
        match error {
            Error::JsonRpc(e) => assert_eq!(e.code, -32007),
            _ => panic!("Expected JsonRpc error"),
        }
    }

    #[test]
    fn test_allow_list_shows_listed_tools() {
        let filter = CapabilityFilter::<Tool>::allow_list(&["query", "list_tables"]);
        let session = SessionState::new();

        assert!(filter.is_visible(&session, &make_test_tool("query")));
        assert!(filter.is_visible(&session, &make_test_tool("list_tables")));
        assert!(!filter.is_visible(&session, &make_test_tool("delete")));
        assert!(!filter.is_visible(&session, &make_test_tool("drop_table")));
    }

    #[test]
    fn test_allow_list_empty_blocks_all() {
        let filter = CapabilityFilter::<Tool>::allow_list(&[]);
        let session = SessionState::new();

        assert!(!filter.is_visible(&session, &make_test_tool("anything")));
    }

    #[test]
    fn test_deny_list_hides_listed_tools() {
        let filter = CapabilityFilter::<Tool>::deny_list(&["delete", "drop_table"]);
        let session = SessionState::new();

        assert!(filter.is_visible(&session, &make_test_tool("query")));
        assert!(filter.is_visible(&session, &make_test_tool("list_tables")));
        assert!(!filter.is_visible(&session, &make_test_tool("delete")));
        assert!(!filter.is_visible(&session, &make_test_tool("drop_table")));
    }

    #[test]
    fn test_deny_list_empty_allows_all() {
        let filter = CapabilityFilter::<Tool>::deny_list(&[]);
        let session = SessionState::new();

        assert!(filter.is_visible(&session, &make_test_tool("anything")));
    }

    #[test]
    fn test_allow_list_with_denial_behavior() {
        let filter = CapabilityFilter::<Tool>::allow_list(&["query"])
            .denial_behavior(DenialBehavior::Unauthorized);
        let session = SessionState::new();

        assert!(!filter.is_visible(&session, &make_test_tool("delete")));
        let error = filter.denial_error("delete");
        match error {
            Error::JsonRpc(e) => assert_eq!(e.code, -32007),
            _ => panic!("Expected JsonRpc error"),
        }
    }
}
