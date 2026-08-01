//! OAuth 2.1 resource server support for MCP.
//!
//! This module implements the resource server side of OAuth 2.1 as specified
//! in the MCP authorization specification. The MCP server acts as
//! a **resource server** -- it validates tokens issued by an external
//! authorization server and serves Protected Resource Metadata for discovery.
//!
//! # Architecture
//!
//! - **Protected Resource Metadata** ([`ProtectedResourceMetadata`]): Served at
//!   `/.well-known/oauth-protected-resource` so OAuth clients can discover
//!   which authorization server to use (RFC 9728).
//!
//! - **Token Validation** ([`TokenValidator`]): Pluggable trait for validating
//!   access tokens. [`JwtValidator`] provides JWT validation with static keys.
//!   [`ValidateAdapter`] bridges the existing [`Validate`](crate::auth::Validate) trait.
//!
//! - **Scope Policy** ([`ScopePolicy`]): Per-operation scope requirements for
//!   tools, resources, and prompts.
//!
//! - **HTTP Middleware** ([`OAuthLayer`]/[`OAuthService`]): Tower middleware that
//!   extracts bearer tokens, validates them, checks scopes, and injects
//!   [`TokenClaims`] into request extensions.
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::{McpRouter, HttpTransport, ToolBuilder, CallToolResult};
//! use tower_mcp::oauth::{
//!     ProtectedResourceMetadata, JwtValidator, ScopePolicy,
//! };
//!
//! #[tokio::main]
//! async fn main() -> Result<(), tower_mcp::BoxError> {
//!     // Define MCP tools
//!     let tool = ToolBuilder::new("echo")
//!         .description("Echo input back")
//!         .handler(|input: serde_json::Value| async move {
//!             Ok(CallToolResult::text(format!("{}", input)))
//!         })
//!         .build();
//!
//!     let router = McpRouter::new()
//!         .server_info("oauth-server", "1.0.0")
//!         .tool(tool);
//!
//!     // Configure OAuth metadata
//!     let metadata = ProtectedResourceMetadata::new("https://mcp.example.com")
//!         .authorization_server("https://auth.example.com")
//!         .scope("mcp:read")
//!         .scope("mcp:write");
//!
//!     // Configure token validator
//!     let validator = JwtValidator::from_secret(b"shared-secret")
//!         .expected_audience("https://mcp.example.com")
//!         .expected_issuer("https://auth.example.com");
//!
//!     // Configure scope policy
//!     let policy = ScopePolicy::new()
//!         .default_scope("mcp:read");
//!
//!     // Build a complete, fail-closed resource server. This validates and
//!     // serves metadata, checks bearer tokens and audience, and applies both
//!     // default and per-operation scope policy.
//!     let app = HttpTransport::new(router)
//!         .into_oauth_router(validator, metadata, policy)?;
//!
//!     let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
//!     axum::serve(listener, app).await?;
//!     Ok(())
//! }
//! ```
//!
//! # Middleware order and scope levels
//!
//! [`HttpTransport::into_oauth_router`](crate::HttpTransport::into_oauth_router)
//! is the recommended setup because it fixes the security-sensitive order:
//! [`OAuthLayer`] is the outer HTTP middleware, the transport bridges validated
//! [`TokenClaims`] into MCP request extensions, and [`ScopeEnforcementLayer`]
//! runs inside the MCP router. The HTTP layer's policy checks scopes that apply
//! to every request; the router layer additionally selects tool-, resource-,
//! and prompt-specific requirements from the same [`ScopePolicy`].
//!
//! For a custom composition, preserve that order and serve
//! [`ProtectedResourceMetadata`] outside authentication. The scope layer is
//! fail closed by default; its explicitly named permissive constructor is only
//! for applications that intentionally mix authenticated and anonymous MCP
//! operations.
//!
//! # Discovery Flow
//!
//! 1. Client requests MCP endpoint without a token
//! 2. Server returns `401` with `WWW-Authenticate: Bearer resource_metadata="..."`
//! 3. Client fetches the resource's RFC 9728 well-known metadata URL
//! 4. Client obtains token from the authorization server
//! 5. Client retries with `Authorization: Bearer <token>`
//!
//! See [`crate::guides::oauth`] for complete resource-server and client setup,
//! registration and persistence policy, identity-provider integration, and a
//! production checklist.

pub mod error;
pub mod metadata;
pub mod middleware;
pub mod scope;
pub mod token;

// Re-exports
pub use error::OAuthError;
pub use metadata::{ProtectedResourceMetadata, ProtectedResourceMetadataError};
pub use middleware::{OAuthLayer, OAuthService};
pub use scope::{
    ScopeEnforcementLayer, ScopeEnforcementService, ScopeMatcher, ScopePolicy, ScopeRequirement,
};
#[cfg(feature = "jwks")]
pub use token::{JwksError, JwksValidator, JwksValidatorBuilder};
pub use token::{JwtValidator, TokenAudience, TokenClaims, TokenValidator, ValidateAdapter};
