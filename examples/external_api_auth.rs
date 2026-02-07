//! External API Authentication Patterns for MCP Tools
//!
//! This example demonstrates common patterns for calling external APIs that require
//! authentication from MCP tool handlers. The right pattern depends on your transport
//! and deployment context.
//!
//! # Transport Considerations
//!
//! ## Local stdio transport
//! - User's environment variables are secure (their own shell)
//! - `extractor_handler` with `State<T>` for shared API client is the natural pattern
//! - No need to bridge identities - the user IS the identity
//!
//! ## Public HTTP/WebSocket transport
//! - Need to authenticate the caller (OAuth, API key, etc.)
//! - Then map that authenticated identity to downstream API credentials
//! - Use `extractor_handler` with `Context` to access the authenticated user's claims
//!
//! # Patterns Demonstrated
//!
//! 1. **Server-side credentials** - Shared API client from env vars (best for stdio)
//! 2. **Per-user from OAuth claims** - Extract downstream token from JWT (best for HTTP)
//! 3. **Elicitation fallback** - Ask user for credentials at runtime
//! 4. **State + Context** - Credential store lookup by user identity
//!
//! Run with: cargo run --example external_api_auth

use std::collections::HashMap;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::RwLock;
use tower_mcp::{
    CallToolResult, McpRouter, StdioTransport, ToolBuilder,
    extract::{Context, Json, State},
    protocol::{ElicitFieldValue, ElicitFormParams, ElicitFormSchema, ElicitMode},
};

// =============================================================================
// Mock API Client (simulates GitHub, Stripe, etc.)
// =============================================================================

/// A mock API client that would normally make HTTP requests to an external API.
/// In real code, this would be something like `octocrab::Octocrab` for GitHub
/// or a Stripe client.
#[derive(Clone)]
struct ExternalApiClient {
    api_key: String,
}

impl ExternalApiClient {
    fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
        }
    }

    /// Simulates an API call that requires authentication
    async fn list_items(&self, query: &str) -> Result<Vec<String>, String> {
        // In real code: self.http_client.get(...).bearer_auth(&self.api_key)...
        if self.api_key.is_empty() {
            return Err("API key required".to_string());
        }
        Ok(vec![
            format!("item-1 for '{}'", query),
            format!("item-2 for '{}'", query),
        ])
    }
}

// =============================================================================
// Input Types
// =============================================================================

#[derive(Debug, Deserialize, JsonSchema)]
struct QueryInput {
    /// Search query
    query: String,
}

// =============================================================================
// Pattern 1: Server-Side Credentials (Best for stdio)
// =============================================================================
//
// The MCP server holds API keys in environment variables. All tool calls
// share the same credentials. This is the simplest pattern and works well
// for local stdio deployments where the user's env vars are trusted.
//
// Pros:
// - Simple, credentials never touch the wire
// - Natural fit for CLI/desktop tools
//
// Cons:
// - No per-user isolation
// - Server is the trust boundary

fn build_server_side_tool(client: Arc<ExternalApiClient>) -> tower_mcp::Tool {
    ToolBuilder::new("search_with_server_key")
        .description(
            "Search using server-side API credentials (Pattern 1: best for stdio transport)",
        )
        .extractor_handler(
            client,
            |State(client): State<Arc<ExternalApiClient>>, Json(input): Json<QueryInput>| async move {
                match client.list_items(&input.query).await {
                    Ok(items) => Ok(CallToolResult::text(format!("Found: {:?}", items))),
                    Err(e) => Ok(CallToolResult::error(format!("API error: {}", e))),
                }
            },
        )
        .build()
}

// =============================================================================
// Pattern 2: Per-User from OAuth Claims (Best for HTTP)
// =============================================================================
//
// Client authenticates to MCP server with OAuth. The JWT contains the user's
// downstream API token (embedded by your auth server during token exchange).
// The tool handler extracts it from the request context.
//
// Pros:
// - Per-user credentials
// - Leverages existing OAuth infrastructure
//
// Cons:
// - Requires OAuth setup
// - Token must carry/enable access to downstream credentials
//
// Note: This pattern requires the `oauth` feature and proper middleware setup.
// In a real implementation with OAuth middleware configured:
//
// ```rust,ignore
// .extractor_handler((), |ctx: Context, Json(input): Json<QueryInput>| async move {
//     let claims = ctx.extensions().get::<TokenClaims>()
//         .ok_or_else(|| Error::tool("Not authenticated"))?;
//     let api_token = claims.extra.get("external_api_token")
//         .and_then(|v| v.as_str())
//         .ok_or_else(|| Error::tool("No API token in claims"))?;
//     let client = ExternalApiClient::new(api_token);
//     // ... use client
// })
// ```
//
// For this example, we demonstrate the pattern with a simulated token:

fn build_oauth_claims_tool() -> tower_mcp::Tool {
    ToolBuilder::new("search_with_user_token")
        .description("Search using per-user API token from OAuth claims (Pattern 2: best for HTTP)")
        .extractor_handler(
            (),
            |_ctx: Context, Json(input): Json<QueryInput>| async move {
                // In production with OAuth middleware, you would extract the token from claims.
                // Here we simulate having extracted a per-user token.
                let api_token = "user-specific-token-from-jwt-claims";

                let client = ExternalApiClient::new(api_token);
                match client.list_items(&input.query).await {
                    Ok(items) => Ok(CallToolResult::text(format!(
                        "Found (user-scoped): {:?}",
                        items
                    ))),
                    Err(e) => Ok(CallToolResult::error(format!("API error: {}", e))),
                }
            },
        )
        .build()
}

// =============================================================================
// Pattern 3: Elicitation Fallback (Interactive)
// =============================================================================
//
// Server requests credentials from the user at runtime via MCP elicitation.
// Good for desktop tools or when credentials aren't pre-configured.
//
// Pros:
// - Just-in-time credential collection
// - User stays in control
//
// Cons:
// - Interrupts the flow
// - Credentials still transit through MCP
// - Requires client support for elicitation

fn build_elicitation_tool() -> tower_mcp::Tool {
    ToolBuilder::new("search_with_elicitation")
        .description("Search by asking user for API key at runtime (Pattern 3: interactive)")
        .extractor_handler(
            (),
            |ctx: Context, Json(input): Json<QueryInput>| async move {
                // Check if elicitation is available (client supports it)
                if !ctx.can_elicit() {
                    return Ok(CallToolResult::error(
                        "This tool requires elicitation support. Please provide credentials another way.",
                    ));
                }

                // Request credentials from the user
                let params = ElicitFormParams {
                    mode: ElicitMode::Form,
                    message: "Please provide your API key to search the external service."
                        .to_string(),
                    requested_schema: ElicitFormSchema::new().string_field(
                        "api_key",
                        Some("Your API key for the external service"),
                        true,
                    ),
                    meta: None,
                };

                match ctx.elicit_form(params).await {
                    Ok(result) => {
                        if let Some(content) = result.content
                            && let Some(ElicitFieldValue::String(api_key)) = content.get("api_key")
                        {
                            let client = ExternalApiClient::new(api_key);
                            match client.list_items(&input.query).await {
                                Ok(items) => {
                                    return Ok(CallToolResult::text(format!(
                                        "Found (user-provided key): {:?}",
                                        items
                                    )));
                                }
                                Err(e) => {
                                    return Ok(CallToolResult::error(format!(
                                        "API error: {}",
                                        e
                                    )));
                                }
                            }
                        }
                        Ok(CallToolResult::error("No API key provided"))
                    }
                    Err(e) => Ok(CallToolResult::error(format!("Elicitation failed: {}", e))),
                }
            },
        )
        .build()
}

// =============================================================================
// Pattern 4: Credential Store with User Context
// =============================================================================
//
// Server maintains a credential store, keyed by user identity from the session.
// Combines shared state (the store) with per-request context (the user ID).
//
// Pros:
// - Credentials stored server-side
// - Per-user isolation
// - Can be backed by any storage (memory, database, vault)
//
// Cons:
// - Server must manage credential lifecycle
// - Need secure storage implementation
//
// In production with OAuth middleware, you would get the user_id from claims:
//
// ```rust,ignore
// .extractor_handler(store, |State(store): State<Arc<CredentialStore>>, ctx: Context, Json(input): Json<QueryInput>| async move {
//     let claims = ctx.extensions().get::<TokenClaims>()?;
//     let user_id = claims.sub.as_ref()?;
//     let token = store.get(user_id, "github").await?;
//     // ... use token
// })
// ```

/// Simple in-memory credential store. In production, back this with
/// a database, HashiCorp Vault, AWS Secrets Manager, etc.
#[derive(Default)]
struct CredentialStore {
    // Maps (user_id, service_name) -> credential
    credentials: RwLock<HashMap<(String, String), String>>,
}

impl CredentialStore {
    fn new() -> Self {
        Self::default()
    }

    async fn set(&self, user_id: &str, service: &str, credential: &str) {
        let mut creds = self.credentials.write().await;
        creds.insert(
            (user_id.to_string(), service.to_string()),
            credential.to_string(),
        );
    }

    async fn get(&self, user_id: &str, service: &str) -> Option<String> {
        let creds = self.credentials.read().await;
        creds
            .get(&(user_id.to_string(), service.to_string()))
            .cloned()
    }
}

fn build_credential_store_tool(store: Arc<CredentialStore>) -> tower_mcp::Tool {
    ToolBuilder::new("search_with_stored_creds")
        .description(
            "Search using credentials from server-side store (Pattern 4: credential store)",
        )
        .extractor_handler(
            store,
            |State(store): State<Arc<CredentialStore>>,
             _ctx: Context,
             Json(input): Json<QueryInput>| async move {
                // In production with OAuth middleware, get user_id from claims:
                // let claims = ctx.extensions().get::<TokenClaims>()?;
                // let user_id = claims.sub.as_ref()?;

                // For this example, we use a fixed demo user
                let user_id = "demo-user";

                // Look up the user's stored credential for this service
                match store.get(user_id, "external_api").await {
                    Some(api_key) => {
                        let client = ExternalApiClient::new(api_key);
                        match client.list_items(&input.query).await {
                            Ok(items) => Ok(CallToolResult::text(format!(
                                "Found (stored creds for {}): {:?}",
                                user_id, items
                            ))),
                            Err(e) => Ok(CallToolResult::error(format!("API error: {}", e))),
                        }
                    }
                    None => Ok(CallToolResult::error(format!(
                        "No credentials stored for user '{}'. Please configure your API key.",
                        user_id
                    ))),
                }
            },
        )
        .build()
}

// =============================================================================
// Main
// =============================================================================

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=debug".parse()?),
        )
        .init();

    // Pattern 1: Server-side credentials from environment
    let api_key = std::env::var("EXTERNAL_API_KEY").unwrap_or_else(|_| "demo-key".to_string());
    let shared_client = Arc::new(ExternalApiClient::new(api_key));

    // Pattern 4: Credential store (pre-populate for demo)
    let cred_store = Arc::new(CredentialStore::new());
    cred_store
        .set("demo-user", "external_api", "stored-api-key-123")
        .await;

    // Build router with all patterns
    let router = McpRouter::new()
        .server_info("external-api-auth-demo", "1.0.0")
        .instructions(
            "Demonstrates authentication patterns for calling external APIs from MCP tools.\n\n\
             - search_with_server_key: Uses server-side env var (Pattern 1)\n\
             - search_with_user_token: Uses per-user OAuth claims (Pattern 2)\n\
             - search_with_elicitation: Asks user for credentials (Pattern 3)\n\
             - search_with_stored_creds: Looks up from credential store (Pattern 4)",
        )
        .tool(build_server_side_tool(shared_client))
        .tool(build_oauth_claims_tool())
        .tool(build_elicitation_tool())
        .tool(build_credential_store_tool(cred_store));

    // Serve over stdio (for local testing)
    // In production, you might use HttpTransport with OAuth middleware for patterns 2-4
    tracing::info!("Starting external API auth demo server over stdio");
    StdioTransport::new(router).run().await?;

    Ok(())
}
