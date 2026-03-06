//! OAuth client example -- authenticated HTTP MCP client
//!
//! Demonstrates three approaches to authenticating an MCP client
//! against a server that requires OAuth/bearer tokens:
//!
//! 1. **Static bearer token** -- simplest, for pre-issued tokens
//! 2. **OAuthClientCredentials** -- client credentials grant with
//!    automatic token caching and refresh
//! 3. **Custom TokenProvider** -- implement your own token logic
//!
//! Run the server first:
//!   cargo run --example http_auth --features oauth -- --auth oauth
//!
//! Then run this client (default: static token):
//!   cargo run --example oauth_client --features oauth-client
//!
//! Or pick an approach:
//!   cargo run --example oauth_client --features oauth-client -- --mode static
//!   cargo run --example oauth_client --features oauth-client -- --mode credentials
//!   cargo run --example oauth_client --features oauth-client -- --mode custom

use async_trait::async_trait;
use tower_mcp::client::{
    HttpClientTransport, McpClient, OAuthClientCredentials, OAuthClientError, TokenProvider,
};

const SERVER_URL: &str = "http://127.0.0.1:3000";

// =============================================================================
// Approach 1: Static bearer token
// =============================================================================

/// The simplest approach -- pass a pre-issued JWT or API token directly.
/// Good for development, CI, or when tokens are managed externally.
async fn demo_static_token() -> Result<(), tower_mcp::BoxError> {
    println!("--- Approach 1: Static Bearer Token ---\n");

    // In production, this would come from an environment variable or secret store.
    // This JWT is signed with the demo secret from the http_auth example:
    //   Header:  {"alg":"HS256","typ":"JWT"}
    //   Payload: {"sub":"demo-user","scope":"mcp:read mcp:write"}
    //   Secret:  demo-secret-do-not-use-in-production
    let token = std::env::var("MCP_TOKEN").unwrap_or_else(|_| {
        println!("  (No MCP_TOKEN env var set, using demo JWT)");
        // Pre-generated JWT for the http_auth example's demo secret
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.\
         eyJzdWIiOiJkZW1vLXVzZXIiLCJzY29wZSI6Im1jcDpyZWFkIG1jcDp3cml0ZSJ9.\
         demo-signature-replace-with-real-jwt"
            .to_string()
    });

    let transport = HttpClientTransport::new(SERVER_URL).bearer_token(&token);

    let client = McpClient::connect(transport).await?;
    let info = client.initialize("oauth-client-static", "1.0.0").await?;
    println!(
        "  Connected to: {} v{}\n",
        info.server_info.name, info.server_info.version
    );

    exercise_server(&client).await?;
    client.shutdown().await?;
    Ok(())
}

// =============================================================================
// Approach 2: OAuthClientCredentials (client credentials grant)
// =============================================================================

/// Machine-to-machine authentication using the OAuth 2.0 client credentials
/// grant. Tokens are automatically cached and refreshed before expiry.
///
/// This is the recommended approach for service-to-service communication.
async fn demo_client_credentials() -> Result<(), tower_mcp::BoxError> {
    println!("--- Approach 2: OAuthClientCredentials ---\n");

    // In production, these come from environment variables or a secret manager.
    let client_id =
        std::env::var("OAUTH_CLIENT_ID").unwrap_or_else(|_| "my-mcp-client".to_string());
    let client_secret =
        std::env::var("OAUTH_CLIENT_SECRET").unwrap_or_else(|_| "my-client-secret".to_string());
    let token_endpoint = std::env::var("OAUTH_TOKEN_ENDPOINT")
        .unwrap_or_else(|_| "https://auth.example.com/oauth/token".to_string());

    println!("  Client ID:      {}", client_id);
    println!("  Token endpoint:  {}", token_endpoint);
    println!();

    let provider = OAuthClientCredentials::builder()
        .client_id(&client_id)
        .client_secret(&client_secret)
        .token_endpoint(&token_endpoint)
        .scopes(["mcp:read", "mcp:write"])
        // Refresh tokens 60 seconds before expiry (default is 30s)
        .refresh_buffer(std::time::Duration::from_secs(60))
        .build()?;

    let transport = HttpClientTransport::new(SERVER_URL).with_token_provider(provider);

    println!("  Connecting (will acquire token on first request)...");
    let client = McpClient::connect(transport).await?;

    // The token will be acquired here, on the first HTTP request
    match client.initialize("oauth-client-credentials", "1.0.0").await {
        Ok(info) => {
            println!(
                "  Connected to: {} v{}\n",
                info.server_info.name, info.server_info.version
            );
            exercise_server(&client).await?;
            client.shutdown().await?;
        }
        Err(e) => {
            println!(
                "  Connection failed (expected if no real OAuth server): {}\n",
                e
            );
            println!("  To test with a real OAuth server, set:");
            println!("    OAUTH_CLIENT_ID, OAUTH_CLIENT_SECRET, OAUTH_TOKEN_ENDPOINT");
        }
    }

    Ok(())
}

// =============================================================================
// Approach 3: Custom TokenProvider
// =============================================================================

/// A custom token provider that rotates between tokens or implements
/// any arbitrary token acquisition logic.
///
/// Use this when you need to integrate with a non-standard auth system,
/// read tokens from a file/vault, or implement custom refresh logic.
struct VaultTokenProvider {
    vault_url: String,
    client: reqwest::Client,
}

impl VaultTokenProvider {
    fn new(vault_url: impl Into<String>) -> Self {
        Self {
            vault_url: vault_url.into(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl TokenProvider for VaultTokenProvider {
    async fn get_token(&self) -> Result<String, OAuthClientError> {
        // In production, this would fetch from HashiCorp Vault, AWS Secrets
        // Manager, or any other secret store.
        println!("  [vault] Fetching token from {}...", self.vault_url);

        // Simulate a vault lookup -- in reality, do an HTTP request:
        //
        // let response = self.client.get(&self.vault_url)
        //     .header("X-Vault-Token", "my-vault-token")
        //     .send()
        //     .await
        //     .map_err(|e| OAuthClientError::TokenRequest(e.to_string()))?;
        //
        // let body: serde_json::Value = response.json().await
        //     .map_err(|e| OAuthClientError::InvalidResponse(e.to_string()))?;
        //
        // body["data"]["token"].as_str()
        //     .map(|s| s.to_string())
        //     .ok_or_else(|| OAuthClientError::InvalidResponse("missing token field".into()))

        // For demo purposes, return a static token
        let _ = &self.client; // suppress unused warning
        Ok("vault-issued-demo-token".to_string())
    }
}

async fn demo_custom_provider() -> Result<(), tower_mcp::BoxError> {
    println!("--- Approach 3: Custom TokenProvider ---\n");

    let provider = VaultTokenProvider::new("https://vault.example.com/v1/secret/mcp-token");

    let transport = HttpClientTransport::new(SERVER_URL).with_token_provider(provider);

    println!("  Connecting (token will be fetched from vault)...");
    let client = McpClient::connect(transport).await?;

    match client.initialize("oauth-client-custom", "1.0.0").await {
        Ok(info) => {
            println!(
                "  Connected to: {} v{}\n",
                info.server_info.name, info.server_info.version
            );
            exercise_server(&client).await?;
            client.shutdown().await?;
        }
        Err(e) => {
            println!(
                "  Connection failed (expected without real server): {}\n",
                e
            );
        }
    }

    Ok(())
}

// =============================================================================
// Shared helper
// =============================================================================

/// Exercise the server by listing tools and calling one.
async fn exercise_server(client: &McpClient) -> Result<(), tower_mcp::BoxError> {
    let tools = client.list_tools().await?;
    println!("  Tools ({}):", tools.tools.len());
    for tool in &tools.tools {
        println!(
            "    - {} : {}",
            tool.name,
            tool.description.as_deref().unwrap_or("(no description)")
        );
    }

    if tools.tools.iter().any(|t| t.name == "add") {
        println!("\n  Calling 'add' with a=17, b=25...");
        let result = client
            .call_tool("add", serde_json::json!({"a": 17, "b": 25}))
            .await?;
        println!("  Result: {}", result.all_text());
    }

    println!();
    Ok(())
}

// =============================================================================
// Main
// =============================================================================

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=warn,oauth_client=debug")
        .init();

    println!("=== MCP OAuth Client Example ===\n");

    let mode = std::env::args()
        .find_map(|a| a.strip_prefix("--mode=").map(String::from))
        .or_else(|| {
            let mut args = std::env::args();
            while let Some(arg) = args.next() {
                if arg == "--mode" {
                    return args.next();
                }
            }
            None
        })
        .unwrap_or_else(|| "static".to_string());

    match mode.as_str() {
        "static" => demo_static_token().await?,
        "credentials" => demo_client_credentials().await?,
        "custom" => demo_custom_provider().await?,
        "all" => {
            demo_static_token().await.ok();
            demo_client_credentials().await.ok();
            demo_custom_provider().await.ok();
        }
        other => {
            eprintln!("Unknown mode: {other}. Use 'static', 'credentials', 'custom', or 'all'.");
            std::process::exit(1);
        }
    }

    println!("=== Done ===");
    Ok(())
}
