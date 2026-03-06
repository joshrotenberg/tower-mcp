//! MCP Conformance Client
//!
//! A client binary for the official MCP conformance test suite.
//! Reads `MCP_CONFORMANCE_SCENARIO` to determine which test to run,
//! and accepts the server URL as the last CLI argument.
//!
//! Run via: `npx @modelcontextprotocol/conformance client --command "cargo run -p conformance-client"`

mod auth_scenarios;
mod core_scenarios;
mod handlers;

use std::env;
use std::process;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "conformance_client=info".into()),
        )
        .with_target(false)
        .init();

    let scenario = env::var("MCP_CONFORMANCE_SCENARIO").unwrap_or_default();
    let server_url = env::args().next_back().unwrap_or_else(|| {
        eprintln!("Usage: conformance-client <server-url>");
        process::exit(1);
    });

    // Parse optional context (auth credentials etc.)
    let context: Option<serde_json::Value> = env::var("MCP_CONFORMANCE_CONTEXT")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());

    tracing::info!(scenario = %scenario, url = %server_url, "Starting conformance client");

    let result = match scenario.as_str() {
        // Core scenarios
        "initialize" => core_scenarios::initialize(&server_url).await,
        "tools_call" => core_scenarios::tools_call(&server_url).await,
        "sse-retry" => core_scenarios::sse_retry(&server_url).await,
        "elicitation-sep1034-client-defaults" | "elicitation-defaults" => {
            core_scenarios::elicitation_defaults(&server_url).await
        }

        // Auth scenarios - metadata discovery
        "auth/metadata-default"
        | "auth/metadata-var1"
        | "auth/metadata-var2"
        | "auth/metadata-var3" => auth_scenarios::standard_auth(&server_url, &context).await,

        // Auth scenarios - CIMD (Client ID Metadata Documents)
        "auth/basic-cimd" => auth_scenarios::standard_auth(&server_url, &context).await,

        // Auth scenarios - scope handling
        "auth/scope-from-www-authenticate"
        | "auth/scope-from-scopes-supported"
        | "auth/scope-omitted-when-undefined" => {
            auth_scenarios::standard_auth(&server_url, &context).await
        }
        "auth/scope-step-up" => auth_scenarios::scope_step_up(&server_url, &context).await,
        "auth/scope-retry-limit" => auth_scenarios::scope_retry_limit(&server_url, &context).await,

        // Auth scenarios - token endpoint auth methods
        "auth/token-endpoint-auth-basic"
        | "auth/token-endpoint-auth-post"
        | "auth/token-endpoint-auth-none" => {
            auth_scenarios::standard_auth(&server_url, &context).await
        }

        // Auth scenarios - resource mismatch
        "auth/resource-mismatch" => auth_scenarios::resource_mismatch(&server_url, &context).await,

        // Auth scenarios - pre-registration
        "auth/pre-registration" => auth_scenarios::pre_registration(&server_url, &context).await,

        // Auth scenarios - client credentials
        "auth/client-credentials-basic" => {
            auth_scenarios::client_credentials_basic(&server_url, &context).await
        }
        "auth/client-credentials-jwt" => {
            auth_scenarios::client_credentials_jwt(&server_url, &context).await
        }

        // Auth scenarios - backcompat
        "auth/2025-03-26-oauth-metadata-backcompat" => {
            auth_scenarios::standard_auth(&server_url, &context).await
        }
        "auth/2025-03-26-oauth-endpoint-fallback" => {
            auth_scenarios::standard_auth(&server_url, &context).await
        }

        // Auth scenarios - cross-app access (SEP-990)
        "auth/cross-app-access-complete-flow" => {
            auth_scenarios::cross_app_access(&server_url, &context).await
        }

        unknown => {
            tracing::warn!(scenario = %unknown, "Unknown scenario, attempting basic client");
            core_scenarios::initialize(&server_url).await
        }
    };

    match result {
        Ok(()) => {
            tracing::info!(scenario = %scenario, "Scenario completed successfully");
            process::exit(0);
        }
        Err(e) => {
            tracing::error!(scenario = %scenario, error = %e, "Scenario failed");
            process::exit(1);
        }
    }
}
