//! HTTP server with authentication middleware example
//!
//! This example demonstrates how to add API key authentication to an MCP server.
//!
//! Run with: cargo run --example http_server_with_auth --features http
//!
//! Test with curl:
//! ```bash
//! # Without auth (fails)
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'
//!
//! # With valid API key
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "Authorization: Bearer sk-test-key-123" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'
//!
//! # With X-API-Key header
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "X-API-Key: sk-test-key-123" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'
//! ```

use axum::{
    Router,
    body::Body,
    extract::Request,
    http::{Response, StatusCode},
    middleware::{self, Next},
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Arc;
use tower_mcp::{
    CallToolResult, HttpTransport, McpRouter, ToolBuilder,
    auth::{AuthError, extract_api_key},
};

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

/// Shared state for authentication
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

/// Auth middleware that checks for API key in Authorization or X-API-Key headers
async fn auth_middleware(
    request: Request,
    next: Next,
) -> Result<Response<Body>, (StatusCode, String)> {
    // Extract API key from headers
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

    // Get the auth state from request extensions
    let auth_state = request
        .extensions()
        .get::<AuthState>()
        .cloned()
        .expect("AuthState not found in extensions");

    // Validate the key
    if let Err(e) = auth_state.validate(key) {
        return Err((
            StatusCode::UNAUTHORIZED,
            serde_json::json!({
                "jsonrpc": "2.0",
                "error": {
                    "code": -32001,
                    "message": e.message
                },
                "id": null
            })
            .to_string(),
        ));
    }

    // Key is valid, proceed with the request
    tracing::info!(
        api_key_prefix = &key[..8.min(key.len())],
        "Authenticated request"
    );
    Ok(next.run(request).await)
}

/// Middleware to inject AuthState into request extensions
async fn inject_auth_state(mut request: Request, next: Next) -> Response<Body> {
    // In a real app, you'd get this from app state
    // For this example, we create it inline
    let auth_state = AuthState::new(vec![
        "sk-test-key-123".to_string(),
        "sk-prod-key-456".to_string(),
    ]);
    request.extensions_mut().insert(auth_state);
    next.run(request).await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=debug".parse()?)
                .add_directive("http_server_with_auth=debug".parse()?),
        )
        .init();

    // Create a simple tool
    let add = ToolBuilder::new("add")
        .description("Add two numbers together")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build()?;

    // Create the MCP router
    let mcp_router = McpRouter::new()
        .server_info("auth-example", "1.0.0")
        .instructions("An MCP server with API key authentication")
        .tool(add);

    // Create the HTTP transport and get its router
    let transport = HttpTransport::new(mcp_router).disable_origin_validation();
    let mcp_axum_router = transport.into_router();

    // Wrap with auth middleware
    // The order matters: inject_auth_state runs first, then auth_middleware
    let app: Router = mcp_axum_router
        .layer(middleware::from_fn(auth_middleware))
        .layer(middleware::from_fn(inject_auth_state));

    // Serve
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    tracing::info!("Starting authenticated HTTP MCP server on http://127.0.0.1:3000");
    tracing::info!("Valid API keys: sk-test-key-123, sk-prod-key-456");
    axum::serve(listener, app).await?;

    Ok(())
}
