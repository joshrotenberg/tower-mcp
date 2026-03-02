//! HTTP authentication examples
//!
//! Demonstrates two approaches to securing an HTTP MCP server:
//!
//!   1. **API key** -- Custom axum middleware validating keys from
//!      Authorization or X-API-Key headers
//!   2. **OAuth 2.1** -- JWT bearer token validation via `OAuthLayer`
//!      with Protected Resource Metadata (RFC 9728)
//!
//! Run with:
//!   API key: cargo run --example http_auth --features oauth -- --auth apikey
//!   OAuth:   cargo run --example http_auth --features oauth -- --auth oauth
//!
//! Test API key auth:
//! ```bash
//! # Without auth (fails)
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'
//!
//! # With valid API key
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "Authorization: Bearer sk-test-key-123" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'
//! ```
//!
//! Test OAuth:
//! ```bash
//! # Discover the authorization server
//! curl http://localhost:3000/.well-known/oauth-protected-resource
//!
//! # Without token (returns 401 with WWW-Authenticate)
//! curl -v -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'
//!
//! # Generate a test JWT at https://jwt.io with:
//! #   Header:  {"alg":"HS256","typ":"JWT"}
//! #   Payload: {"sub":"demo-user","scope":"mcp:read mcp:write"}
//! #   Secret:  demo-secret-do-not-use-in-production
//! ```

use axum::Router;
use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

fn build_mcp_router() -> McpRouter {
    let add = ToolBuilder::new("add")
        .description("Add two numbers together")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    McpRouter::new()
        .server_info("auth-example", "1.0.0")
        .instructions("An MCP server with authentication")
        .tool(add)
}

// ---------------------------------------------------------------------------
// API Key auth: custom axum middleware
// ---------------------------------------------------------------------------

mod apikey {
    use axum::{
        Router,
        body::Body,
        extract::Request,
        http::{Response, StatusCode},
        middleware::{self, Next},
    };
    use std::collections::HashSet;
    use std::sync::Arc;
    use tower_mcp::auth::{AuthError, extract_api_key};

    #[derive(Clone)]
    struct AuthState {
        valid_keys: Arc<HashSet<String>>,
    }

    impl AuthState {
        fn new(keys: Vec<String>) -> Self {
            Self {
                valid_keys: Arc::new(keys.into_iter().collect()),
            }
        }

        fn validate(&self, key: &str) -> Result<(), AuthError> {
            if self.valid_keys.contains(key) {
                Ok(())
            } else {
                Err(AuthError {
                    code: "invalid_api_key".to_string(),
                    message: "The provided API key is not valid".to_string(),
                })
            }
        }
    }

    async fn auth_middleware(
        request: Request,
        next: Next,
    ) -> Result<Response<Body>, (StatusCode, String)> {
        let api_key = request
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(extract_api_key)
            .or_else(|| {
                request
                    .headers()
                    .get("X-API-Key")
                    .and_then(|v| v.to_str().ok())
            });

        let Some(key) = api_key else {
            return Err((
                StatusCode::UNAUTHORIZED,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "error": {
                        "code": -32001,
                        "message": "Missing API key. Provide via Authorization or X-API-Key header."
                    },
                    "id": null
                })
                .to_string(),
            ));
        };

        let auth_state = request
            .extensions()
            .get::<AuthState>()
            .cloned()
            .expect("AuthState not found in extensions");

        if let Err(e) = auth_state.validate(key) {
            return Err((
                StatusCode::UNAUTHORIZED,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "error": { "code": -32001, "message": e.message },
                    "id": null
                })
                .to_string(),
            ));
        }

        tracing::info!(api_key_prefix = &key[..8.min(key.len())], "Authenticated");
        Ok(next.run(request).await)
    }

    async fn inject_auth_state(mut request: Request, next: Next) -> Response<Body> {
        let auth_state = AuthState::new(vec![
            "sk-test-key-123".to_string(),
            "sk-prod-key-456".to_string(),
        ]);
        request.extensions_mut().insert(auth_state);
        next.run(request).await
    }

    pub fn wrap(mcp_router: Router) -> Router {
        mcp_router
            .layer(middleware::from_fn(auth_middleware))
            .layer(middleware::from_fn(inject_auth_state))
    }
}

// ---------------------------------------------------------------------------
// OAuth 2.1: JWT validation via OAuthLayer
// ---------------------------------------------------------------------------

mod oauth {
    use axum::Router;
    use tower_mcp::oauth::{JwtValidator, OAuthLayer, ProtectedResourceMetadata, ScopePolicy};

    pub fn wrap(mcp_router: Router, metadata: ProtectedResourceMetadata) -> Router {
        // In production, use RSA/EC keys or JWKS endpoint instead of a shared secret
        let validator = JwtValidator::from_secret(b"demo-secret-do-not-use-in-production")
            .disable_exp_validation(); // For demo convenience only

        let scope_policy = ScopePolicy::new().default_scope("mcp:read");

        let oauth_layer = OAuthLayer::new(validator, metadata).scope_policy(scope_policy);

        mcp_router.layer(oauth_layer)
    }
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=debug".parse()?)
                .add_directive("http_auth=debug".parse()?),
        )
        .init();

    let auth_mode = std::env::args()
        .find_map(|a| a.strip_prefix("--auth=").map(String::from))
        .or_else(|| {
            let mut args = std::env::args();
            while let Some(arg) = args.next() {
                if arg == "--auth" {
                    return args.next();
                }
            }
            None
        })
        .unwrap_or_else(|| "apikey".to_string());

    let mcp_router = build_mcp_router();

    let app: Router = match auth_mode.as_str() {
        "apikey" => {
            let transport = HttpTransport::new(mcp_router).disable_origin_validation();
            let mcp_axum_router = transport.into_router();
            tracing::info!("Valid API keys: sk-test-key-123, sk-prod-key-456");
            apikey::wrap(mcp_axum_router)
        }
        "oauth" => {
            use tower_mcp::oauth::ProtectedResourceMetadata;

            let metadata = ProtectedResourceMetadata::new("http://localhost:3000")
                .authorization_server("https://auth.example.com")
                .scope("mcp:read")
                .scope("mcp:write")
                .resource_documentation("https://github.com/joshrotenberg/tower-mcp");

            let transport = HttpTransport::new(mcp_router)
                .disable_origin_validation()
                .oauth(metadata.clone());
            let mcp_axum_router = transport.into_router();

            tracing::info!(
                "Protected Resource Metadata: http://127.0.0.1:3000/.well-known/oauth-protected-resource"
            );
            oauth::wrap(mcp_axum_router, metadata)
        }
        other => {
            eprintln!("Unknown auth mode: {other}. Use 'apikey' or 'oauth'.");
            std::process::exit(1);
        }
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    tracing::info!("Starting HTTP MCP server with {auth_mode} auth on http://127.0.0.1:3000");
    axum::serve(listener, app).await?;

    Ok(())
}
