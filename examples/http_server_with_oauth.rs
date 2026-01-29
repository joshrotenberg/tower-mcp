//! HTTP server with OAuth 2.1 resource server support
//!
//! Demonstrates:
//! - Protected Resource Metadata at `/.well-known/oauth-protected-resource`
//! - JWT bearer token validation via `OAuthLayer`
//! - Scope-based access control with `ScopePolicy`
//!
//! Run with: cargo run --example http_server_with_oauth --features oauth
//!
//! Test with curl:
//!
//! ```bash
//! # 1. Discover the authorization server (public endpoint)
//! curl http://localhost:3000/.well-known/oauth-protected-resource
//!
//! # 2. Attempt without token (returns 401 with WWW-Authenticate header)
//! curl -v -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'
//!
//! # 3. Generate a test JWT (in a real app, obtained from the auth server):
//! #    The example uses the shared secret "demo-secret-do-not-use-in-production"
//! #    You can generate a test token at https://jwt.io with:
//! #    Header: {"alg":"HS256","typ":"JWT"}
//! #    Payload: {"sub":"demo-user","scope":"mcp:read mcp:write"}
//! #    Secret: demo-secret-do-not-use-in-production
//!
//! # 4. Request with valid token
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "Authorization: Bearer <your-jwt-here>" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'
//! ```

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::oauth::{JwtValidator, OAuthLayer, ProtectedResourceMetadata, ScopePolicy};
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=debug".parse()?)
                .add_directive("http_server_with_oauth=debug".parse()?),
        )
        .init();

    // Create tools
    let add = ToolBuilder::new("add")
        .description("Add two numbers together")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build()?;

    let router = McpRouter::new()
        .server_info("oauth-example", "1.0.0")
        .instructions("An MCP server with OAuth 2.1 resource server support")
        .tool(add);

    // Configure Protected Resource Metadata (RFC 9728)
    let metadata = ProtectedResourceMetadata::new("http://localhost:3000")
        .authorization_server("https://auth.example.com")
        .scope("mcp:read")
        .scope("mcp:write")
        .resource_documentation("https://github.com/joshrotenberg/tower-mcp");

    // Configure JWT validator
    // In production, use RSA/EC keys or JWKS endpoint instead of a shared secret
    let validator =
        JwtValidator::from_secret(b"demo-secret-do-not-use-in-production").disable_exp_validation(); // For demo convenience only

    // Configure scope policy
    let scope_policy = ScopePolicy::new().default_scope("mcp:read");

    // Build OAuth middleware layer
    let oauth_layer = OAuthLayer::new(validator, metadata.clone()).scope_policy(scope_policy);

    // Build transport with metadata endpoint and apply OAuth middleware
    let transport = HttpTransport::new(router)
        .disable_origin_validation()
        .oauth(metadata);

    let app = transport.into_router().layer(oauth_layer);

    // Serve
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    tracing::info!("OAuth MCP server listening on http://127.0.0.1:3000");
    tracing::info!(
        "Protected Resource Metadata: http://127.0.0.1:3000/.well-known/oauth-protected-resource"
    );
    axum::serve(listener, app).await?;

    Ok(())
}
