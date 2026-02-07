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

use std::sync::Arc;

use crate::error::{Error, JsonRpcError};
use crate::prompt::Prompt;
use crate::resource::Resource;
use crate::session::SessionState;
use crate::tool::Tool;

/// Trait for capabilities that can be filtered by session.
///
/// Implemented for [`Tool`], [`Resource`], and [`Prompt`].
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

impl Filterable for Prompt {
    fn name(&self) -> &str {
        &self.name
    }
}

/// Behavior when a filtered capability is accessed directly.
#[derive(Clone, Default)]
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
    filter: Arc<dyn Fn(&SessionState, &T) -> bool + Send + Sync>,
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
        (self.filter)(session, capability)
    }

    /// Get the error to return when access is denied.
    pub fn denial_error(&self, name: &str) -> Error {
        self.denial.to_error(name)
    }
}

/// Type alias for tool filters.
pub type ToolFilter = CapabilityFilter<Tool>;

/// Type alias for resource filters.
pub type ResourceFilter = CapabilityFilter<Resource>;

/// Type alias for prompt filters.
pub type PromptFilter = CapabilityFilter<Prompt>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CallToolResult;
    use crate::tool::ToolBuilder;

    fn make_test_tool(name: &str) -> Tool {
        ToolBuilder::new(name)
            .description("Test tool")
            .handler(|_: serde_json::Value| async { Ok(CallToolResult::text("ok")) })
            .build()
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
}
