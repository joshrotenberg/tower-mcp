//! OAuth 2.1 resource server support for MCP.
//!
//! This module implements the resource server side of OAuth 2.1 as specified
//! in the MCP 2025-11-25 authentication specification. The MCP server acts as
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
//!     ProtectedResourceMetadata, JwtValidator, OAuthLayer, ScopePolicy,
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
//!         .build()?;
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
//!     // Build OAuth middleware layer
//!     let oauth = OAuthLayer::new(validator, metadata.clone())
//!         .scope_policy(policy);
//!
//!     // Build transport with metadata endpoint
//!     let transport = HttpTransport::new(router)
//!         .oauth(metadata);
//!
//!     // Apply OAuth middleware to the axum router
//!     let app = transport.into_router().layer(oauth);
//!
//!     let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
//!     axum::serve(listener, app).await?;
//!     Ok(())
//! }
//! ```
//!
//! # Discovery Flow
//!
//! 1. Client requests MCP endpoint without a token
//! 2. Server returns `401` with `WWW-Authenticate: Bearer resource_metadata="..."`
//! 3. Client fetches `/.well-known/oauth-protected-resource` to discover auth server
//! 4. Client obtains token from the authorization server
//! 5. Client retries with `Authorization: Bearer <token>`

pub mod error;
pub mod metadata;
pub mod middleware;
pub mod scope;
pub mod token;

// Re-exports
pub use error::OAuthError;
pub use metadata::ProtectedResourceMetadata;
pub use middleware::{OAuthLayer, OAuthService};
pub use scope::{ScopeEnforcementLayer, ScopeEnforcementService, ScopePolicy, ScopeRequirement};
pub use token::{JwtValidator, TokenAudience, TokenClaims, TokenValidator, ValidateAdapter};
