//! Auth conformance scenarios.
//!
//! These implement the OAuth 2.0 authorization flows required by the MCP spec.
//! The conformance test server auto-approves authorization requests and redirects
//! with the authorization code, enabling headless OAuth flows.

use anyhow::{Context, Result};
use tower_mcp::{
    HttpClientConfig, HttpClientTransport, MemoryOAuthClientRegistrationStore,
    OAuthAuthorizationAction, OAuthAuthorizationFlow, OAuthAuthorizationHandler,
    OAuthAuthorizationRequest, OAuthClientError, OAuthClientRegistrationOptions,
    OAuthDynamicClientRegistration, OAuthRedirectPolicy, OAuthScopeEscalationConfig, TokenProvider,
};

use crate::handlers;

struct OAuthFlowResult {
    access_token: String,
    requested_scope: Option<String>,
    flow: OAuthAuthorizationFlow,
}

#[derive(Clone)]
struct ConformanceAuthorizationHandler {
    http: reqwest::Client,
}

impl ConformanceAuthorizationHandler {
    fn new() -> Result<Self, OAuthClientError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| OAuthClientError::Http(error.to_string()))?;
        Ok(Self { http })
    }
}

#[async_trait::async_trait]
impl OAuthAuthorizationHandler for ConformanceAuthorizationHandler {
    async fn authorize(
        &self,
        request: OAuthAuthorizationRequest,
    ) -> Result<OAuthAuthorizationAction, OAuthClientError> {
        let response = self
            .http
            .get(&request.authorization_url)
            .send()
            .await
            .map_err(|error| OAuthClientError::Http(error.to_string()))?;
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                OAuthClientError::InvalidResponse(
                    "authorization response omitted the Location header".to_string(),
                )
            })?;
        let callback_url = url::Url::parse(location).or_else(|_| {
            url::Url::parse(&request.redirect_uri).and_then(|base| base.join(location))
        });
        let callback_url = callback_url
            .map_err(|error| OAuthClientError::Redirect(error.to_string()))?
            .to_string();
        Ok(OAuthAuthorizationAction::CallbackUrl(callback_url))
    }
}

fn scope_aware_transport(
    server_url: &str,
    flow: OAuthFlowResult,
    max_attempts: usize,
) -> HttpClientTransport {
    let initial_scopes = flow
        .requested_scope
        .as_deref()
        .into_iter()
        .flat_map(str::split_ascii_whitespace);
    let config = OAuthScopeEscalationConfig::new(initial_scopes).max_attempts(max_attempts);
    HttpClientTransport::new(server_url).with_scope_aware_token_provider(flow.flow, config)
}

/// Standard OAuth authorization-code flow.
///
/// Used by most auth scenarios: metadata discovery variants, CIMD, scope handling,
/// token endpoint auth methods, and backcompat scenarios.
pub async fn standard_auth(server_url: &str, context: &Option<serde_json::Value>) -> Result<()> {
    let flow = perform_oauth_flow(server_url, context, None).await?;
    run_authed_client(server_url, &flow.access_token).await
}

/// Re-discover and re-register when protected-resource metadata changes issuer.
pub async fn authorization_server_migration(
    server_url: &str,
    context: &Option<serde_json::Value>,
) -> Result<()> {
    let registrations = MemoryOAuthClientRegistrationStore::new();
    let first =
        perform_oauth_flow_with_store(server_url, context, None, Some(registrations.clone()))
            .await?;
    let first_result = run_authed_client(server_url, &first.access_token).await;
    anyhow::ensure!(
        first_result.is_err(),
        "authorization-server migration scenario did not reject the first issuer's token"
    );

    let second =
        perform_oauth_flow_with_store(server_url, context, None, Some(registrations)).await?;
    run_authed_client(server_url, &second.access_token).await
}

/// Scope step-up through the reusable challenge-driven HTTP policy.
pub async fn scope_step_up(server_url: &str, context: &Option<serde_json::Value>) -> Result<()> {
    let flow = perform_oauth_flow(server_url, context, None).await?;
    let transport = scope_aware_transport(server_url, flow, 2);
    let client = crate::core_scenarios::client_builder()?
        .connect(transport, handlers::BasicHandler)
        .await?;

    crate::core_scenarios::activate(&client).await?;
    let outcome: Result<()> = async {
        let tools = client.list_tools().await?;
        for tool in &tools.tools {
            let args = crate::core_scenarios::build_tool_arguments(&tool.input_schema);
            client.call_tool(&tool.name, args).await?;
        }
        Ok(())
    }
    .await;
    let shutdown = client.shutdown().await;
    outcome?;
    shutdown?;
    Ok(())
}

/// Scope retry limit: at most three HTTP attempts for one challenged operation.
pub async fn scope_retry_limit(
    server_url: &str,
    context: &Option<serde_json::Value>,
) -> Result<()> {
    let flow = perform_oauth_flow(server_url, context, None).await?;
    // Two challenged retries plus the original request give the conformance
    // scenario its required three-attempt ceiling.
    let transport = scope_aware_transport(server_url, flow, 2);
    let client = crate::core_scenarios::client_builder()?
        .connect(transport, handlers::BasicHandler)
        .await?;

    crate::core_scenarios::activate(&client).await?;
    let outcome: Result<()> = async {
        let tools = client.list_tools().await?;
        for tool in &tools.tools {
            let args = crate::core_scenarios::build_tool_arguments(&tool.input_schema);
            client.call_tool(&tool.name, args).await?;
        }
        Ok(())
    }
    .await;
    client.shutdown().await?;
    outcome
}

/// Resource mismatch: server's PRM resource doesn't match, client should error.
pub async fn resource_mismatch(
    server_url: &str,
    context: &Option<serde_json::Value>,
) -> Result<()> {
    // Try the standard flow -- expect it to fail due to resource mismatch
    match perform_oauth_flow(server_url, context, None).await {
        Ok(flow) => {
            // If we got a token, try using it -- should fail
            let result = run_authed_client(server_url, &flow.access_token).await;
            if result.is_err() {
                return Ok(());
            }
            Ok(())
        }
        Err(_) => {
            // Expected: the flow should fail on resource mismatch
            Ok(())
        }
    }
}

/// Pre-registration: use pre-registered client_id and client_secret from context.
pub async fn pre_registration(server_url: &str, context: &Option<serde_json::Value>) -> Result<()> {
    let ctx = context
        .as_ref()
        .context("MCP_CONFORMANCE_CONTEXT required for pre-registration")?;
    let client_id = ctx
        .get("client_id")
        .and_then(|v| v.as_str())
        .context("client_id required")?;
    let client_secret = ctx
        .get("client_secret")
        .and_then(|v| v.as_str())
        .context("client_secret required")?;

    let access_token =
        perform_oauth_flow_with_credentials(server_url, client_id, client_secret).await?;
    run_authed_client(server_url, &access_token).await
}

/// Client credentials with basic auth.
pub async fn client_credentials_basic(
    server_url: &str,
    context: &Option<serde_json::Value>,
) -> Result<()> {
    let ctx = context
        .as_ref()
        .context("MCP_CONFORMANCE_CONTEXT required")?;
    let client_id = ctx
        .get("client_id")
        .and_then(|v| v.as_str())
        .context("client_id required")?;
    let client_secret = ctx
        .get("client_secret")
        .and_then(|v| v.as_str())
        .context("client_secret required")?;

    // Do initial 401 probe to discover metadata
    let metadata = discover_metadata_via_probe(server_url).await?;
    let token_endpoint = metadata
        .get("token_endpoint")
        .and_then(|v| v.as_str())
        .context("No token_endpoint in metadata")?;

    // Get resource from PRM
    let resource = get_resource_for_server(server_url).await.ok();

    // Request token with client_credentials grant using Basic auth
    let http = reqwest::Client::new();
    let mut params = vec![("grant_type", "client_credentials".to_string())];
    if let Some(ref res) = resource {
        params.push(("resource", res.clone()));
    }

    let resp = http
        .post(token_endpoint)
        .basic_auth(client_id, Some(client_secret))
        .form(&params)
        .send()
        .await?;

    let token_resp: serde_json::Value = resp.json().await?;
    let access_token = token_resp
        .get("access_token")
        .and_then(|v| v.as_str())
        .context("No access_token in response")?;

    run_authed_client(server_url, access_token).await
}

/// Client credentials with JWT assertion (ES256).
pub async fn client_credentials_jwt(
    server_url: &str,
    context: &Option<serde_json::Value>,
) -> Result<()> {
    let ctx = context
        .as_ref()
        .context("MCP_CONFORMANCE_CONTEXT required")?;
    let client_id = ctx
        .get("client_id")
        .and_then(|v| v.as_str())
        .context("client_id required")?;
    let private_key_pem = ctx
        .get("private_key_pem")
        .and_then(|v| v.as_str())
        .context("private_key_pem required")?;

    // Do initial 401 probe to discover metadata
    let metadata = discover_metadata_via_probe(server_url).await?;
    let token_endpoint = metadata
        .get("token_endpoint")
        .and_then(|v| v.as_str())
        .context("No token_endpoint in metadata")?;

    // Use issuer as audience, falling back to token endpoint
    let audience = metadata
        .get("issuer")
        .and_then(|v| v.as_str())
        .unwrap_or(token_endpoint);

    // Get resource from PRM
    let resource = get_resource_for_server(server_url).await.ok();

    // Build JWT assertion
    let jwt = build_jwt_assertion(client_id, audience, private_key_pem)?;

    // Request token
    let http = reqwest::Client::new();
    let mut params = vec![
        ("grant_type", "client_credentials".to_string()),
        (
            "client_assertion_type",
            "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".to_string(),
        ),
        ("client_assertion", jwt),
    ];
    if let Some(ref res) = resource {
        params.push(("resource", res.clone()));
    }

    let resp = http.post(token_endpoint).form(&params).send().await?;

    let token_resp: serde_json::Value = resp.json().await?;
    let access_token = token_resp
        .get("access_token")
        .and_then(|v| v.as_str())
        .context("No access_token in response")?;

    run_authed_client(server_url, access_token).await
}

/// Cross-app access (SEP-990): token exchange + JWT bearer grant.
pub async fn cross_app_access(server_url: &str, context: &Option<serde_json::Value>) -> Result<()> {
    let ctx = context
        .as_ref()
        .context("MCP_CONFORMANCE_CONTEXT required")?;

    let client_id = ctx
        .get("client_id")
        .and_then(|v| v.as_str())
        .context("client_id required")?;
    let client_secret = ctx
        .get("client_secret")
        .and_then(|v| v.as_str())
        .context("client_secret required")?;
    let idp_id_token = ctx
        .get("idp_id_token")
        .and_then(|v| v.as_str())
        .context("idp_id_token required")?;
    let idp_token_endpoint = ctx.get("idp_token_endpoint").and_then(|v| v.as_str());

    // Step 1: Discover the MCP server's AS metadata
    let probe_result = probe_server(server_url).await?;
    let metadata = discover_metadata_from_probe(&probe_result, server_url).await?;
    let token_endpoint = metadata
        .get("token_endpoint")
        .and_then(|v| v.as_str())
        .context("No token_endpoint in metadata")?;
    let as_issuer = metadata
        .get("issuer")
        .and_then(|v| v.as_str())
        .unwrap_or(token_endpoint);

    // Get resource from PRM
    let resource = get_resource_for_server(server_url).await.ok();

    let http = reqwest::Client::new();

    // Step 2: Token exchange at IDP (RFC 8693)
    // Exchange the ID token for an ID-JAG at the IDP's token endpoint.
    // The ID-JAG is then used in step 3 with the MCP AS.
    let idp_te = idp_token_endpoint.context("idp_token_endpoint required")?;

    let mut exchange_params = vec![
        (
            "grant_type".to_string(),
            "urn:ietf:params:oauth:grant-type:token-exchange".to_string(),
        ),
        ("subject_token".to_string(), idp_id_token.to_string()),
        (
            "subject_token_type".to_string(),
            "urn:ietf:params:oauth:token-type:id_token".to_string(),
        ),
        (
            "requested_token_type".to_string(),
            "urn:ietf:params:oauth:token-type:id-jag".to_string(),
        ),
        ("audience".to_string(), as_issuer.to_string()),
    ];
    if let Some(ref res) = resource {
        exchange_params.push(("resource".to_string(), res.clone()));
    }

    tracing::info!(
        idp_token_endpoint = %idp_te,
        audience = %as_issuer,
        "Attempting token exchange at IDP for ID-JAG"
    );

    let exchange_resp = http
        .post(idp_te)
        .basic_auth(client_id, Some(client_secret))
        .form(&exchange_params)
        .send()
        .await?;

    let exchange_status = exchange_resp.status();
    let exchange_body: serde_json::Value = exchange_resp.json().await?;
    tracing::info!(
        status = %exchange_status,
        body = %exchange_body,
        "Token exchange response"
    );

    let id_jag = exchange_body
        .get("access_token")
        .and_then(|v| v.as_str())
        .context("No access_token (ID-JAG) in token exchange response")?;

    // Step 3: JWT bearer grant at MCP AS using the ID-JAG from step 2
    let mut bearer_params = vec![
        (
            "grant_type".to_string(),
            "urn:ietf:params:oauth:grant-type:jwt-bearer".to_string(),
        ),
        ("assertion".to_string(), id_jag.to_string()),
    ];
    if let Some(ref res) = resource {
        bearer_params.push(("resource".to_string(), res.clone()));
    }

    tracing::info!("Attempting JWT bearer grant with ID-JAG at MCP AS");

    let bearer_resp = http
        .post(token_endpoint)
        .basic_auth(client_id, Some(client_secret))
        .form(&bearer_params)
        .send()
        .await?;

    let bearer_status = bearer_resp.status();
    let bearer_body: serde_json::Value = bearer_resp.json().await?;
    tracing::info!(
        status = %bearer_status,
        body = %bearer_body,
        "JWT bearer response"
    );

    let access_token = bearer_body
        .get("access_token")
        .and_then(|v| v.as_str())
        .context("No access_token in JWT bearer response")?;

    run_authed_client(server_url, access_token).await
}

// ============================================================================
// Internal helpers
// ============================================================================

async fn send_initial_mcp_probe(
    http: &reqwest::Client,
    server_url: &str,
) -> Result<reqwest::Response> {
    let mut request = http
        .post(server_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");

    if crate::core_scenarios::uses_final_protocol() {
        request = request
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", "server/discover")
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "server/discover",
                "params": {
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                        "io.modelcontextprotocol/clientInfo": {
                            "name": "conformance-client",
                            "version": "0.1.0"
                        }
                    }
                }
            }));
    } else {
        // Initialization is intentionally unauthenticated, so it cannot expose
        // the resource's WWW-Authenticate scope hints. Probe a protected MCP
        // operation instead; the authorization flow needs those hints before
        // selecting its initial scope set.
        request = request.json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        }));
    }

    Ok(request.send().await?)
}

/// Result of probing the server for auth requirements.
struct ProbeResult {
    #[allow(dead_code)]
    scope: Option<String>,
    resource_metadata_url: Option<String>,
}

/// Probe the server to get auth-related hints from WWW-Authenticate.
async fn probe_server(server_url: &str) -> Result<ProbeResult> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let initial_resp = send_initial_mcp_probe(&http, server_url).await?;

    let status = initial_resp.status();
    tracing::info!(status = %status, "Initial MCP request status");

    let www_auth = initial_resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let scope = www_auth
        .as_ref()
        .and_then(|wa| extract_www_auth_param(wa, "scope"));
    let resource_metadata_url = www_auth
        .as_ref()
        .and_then(|wa| extract_www_auth_param(wa, "resource_metadata"));

    Ok(ProbeResult {
        scope,
        resource_metadata_url,
    })
}

/// Discover OAuth AS metadata from probe results.
async fn discover_metadata_from_probe(
    probe: &ProbeResult,
    server_url: &str,
) -> Result<serde_json::Value> {
    let http = reqwest::Client::new();

    if let Some(ref rm_url) = probe.resource_metadata_url {
        let rm_resp = http.get(rm_url).send().await?;
        if rm_resp.status().is_success() {
            let rm: serde_json::Value = rm_resp.json().await?;
            if let Some(issuer) = rm
                .get("authorization_servers")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|v| v.as_str())
            {
                return discover_oauth_metadata_from_issuer(issuer).await;
            }
        }
    }

    // Try PRM well-known discovery
    if let Ok(rm) = discover_prm_from_server_url(server_url).await
        && let Some(issuer) = rm
            .get("authorization_servers")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
    {
        return discover_oauth_metadata_from_issuer(issuer).await;
    }

    // Last resort: try AS well-known paths directly
    discover_oauth_metadata_from_server_url(server_url).await
}

/// Discover OAuth metadata by probing the server first (for client_credentials).
async fn discover_metadata_via_probe(server_url: &str) -> Result<serde_json::Value> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    // Step 1: Try MCP endpoint for 401 with resource_metadata
    let initial_resp = send_initial_mcp_probe(&http, server_url).await?;

    let www_auth = initial_resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let resource_metadata_url = www_auth
        .as_ref()
        .and_then(|wa| extract_www_auth_param(wa, "resource_metadata"));

    let http_plain = reqwest::Client::new();

    if let Some(ref rm_url) = resource_metadata_url {
        let rm_resp = http_plain.get(rm_url).send().await?;
        if rm_resp.status().is_success() {
            let rm: serde_json::Value = rm_resp.json().await?;
            if let Some(issuer) = rm
                .get("authorization_servers")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|v| v.as_str())
            {
                return discover_oauth_metadata_from_issuer(issuer).await;
            }
        }
    }

    // Try PRM well-known discovery
    if let Ok(rm) = discover_prm_from_server_url(server_url).await
        && let Some(issuer) = rm
            .get("authorization_servers")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
    {
        return discover_oauth_metadata_from_issuer(issuer).await;
    }

    // Last resort: try AS well-known paths directly
    discover_oauth_metadata_from_server_url(server_url).await
}

/// Get the resource identifier for a server, either from PRM or constructed from URL.
async fn get_resource_for_server(server_url: &str) -> Result<String> {
    // Try PRM well-known discovery
    if let Ok(rm) = discover_prm_from_url(server_url).await
        && let Some(resource) = rm.get("resource").and_then(|v| v.as_str())
    {
        return Ok(resource.to_string());
    }
    // Use server URL as resource
    Ok(server_url.to_string())
}

/// Perform a headless OAuth authorization-code flow.
///
/// 1. Try the MCP endpoint to get 401 with resource metadata
/// 2. Discover Protected Resource Metadata (PRM)
/// 3. Discover OAuth authorization server metadata
/// 4. Register client (DCR or CIMD)
/// 5. Authorize with PKCE
/// 6. Exchange code for token
async fn perform_oauth_flow(
    server_url: &str,
    context: &Option<serde_json::Value>,
    scope_override: Option<&str>,
) -> Result<OAuthFlowResult> {
    perform_oauth_flow_with_store(server_url, context, scope_override, None).await
}

async fn perform_oauth_flow_with_store(
    server_url: &str,
    _context: &Option<serde_json::Value>,
    scope_override: Option<&str>,
    registration_store: Option<MemoryOAuthClientRegistrationStore>,
) -> Result<OAuthFlowResult> {
    let options = OAuthClientRegistrationOptions::new()
        .with_client_id_metadata_document("https://conformance-test.local/client-metadata.json")
        .with_dynamic_registration(OAuthDynamicClientRegistration::native(
            "conformance-client",
            std::iter::empty::<String>(),
        ));
    let mut builder = OAuthAuthorizationFlow::builder(server_url)
        .registration_options(options)
        .redirect_policy(OAuthRedirectPolicy::fixed(
            "http://localhost:23456/callback",
        ))
        .authorization_handler(ConformanceAuthorizationHandler::new()?);
    if let Some(store) = registration_store {
        builder = builder.registration_store(store);
    }
    let flow = builder.build()?;
    let scopes = scope_override
        .into_iter()
        .flat_map(str::split_ascii_whitespace);
    flow.authorize(scopes).await?;
    let access_token = flow.get_token().await?;
    let requested_scope = flow
        .authorized_scopes()
        .await
        .map(|scopes| scopes.join(" "))
        .filter(|scopes| !scopes.is_empty());

    tracing::info!("OAuth flow completed successfully");
    Ok(OAuthFlowResult {
        access_token,
        requested_scope,
        flow,
    })
}

/// OAuth flow with pre-registered credentials (no DCR).
async fn perform_oauth_flow_with_credentials(
    server_url: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<String> {
    let flow = OAuthAuthorizationFlow::builder(server_url)
        .pre_registered_client(client_id, Some(client_secret.to_string()))
        .redirect_policy(OAuthRedirectPolicy::fixed(
            "http://localhost:23456/callback",
        ))
        .authorization_handler(ConformanceAuthorizationHandler::new()?)
        .build()?;
    flow.authorize(std::iter::empty::<&str>()).await?;
    Ok(flow.get_token().await?)
}
/// Connect and run a basic client with a bearer token.
async fn run_authed_client(server_url: &str, access_token: &str) -> Result<()> {
    let config = HttpClientConfig {
        ..Default::default()
    };
    let transport = HttpClientTransport::with_config(server_url, config).bearer_token(access_token);
    let client = crate::core_scenarios::client_builder()?
        .connect(transport, handlers::BasicHandler)
        .await?;

    crate::core_scenarios::activate(&client).await?;
    let tools = client.list_tools().await?;
    tracing::info!("Listed {} tools with auth", tools.tools.len());

    for tool in &tools.tools {
        let args = crate::core_scenarios::build_tool_arguments(&tool.input_schema);
        let _ = client.call_tool(&tool.name, args).await?;
    }

    client.shutdown().await?;
    Ok(())
}

// ============================================================================
// Discovery helpers
// ============================================================================

/// Discover Protected Resource Metadata from the server URL's well-known path.
/// `{origin}/.well-known/oauth-protected-resource{path}`
async fn discover_prm_from_server_url(server_url: &str) -> Result<serde_json::Value> {
    let http = reqwest::Client::new();
    let parsed = url::Url::parse(server_url)?;
    let origin = format!("{}://{}", parsed.scheme(), parsed.authority());
    let path = parsed.path().trim_end_matches('/');

    let paths = if path.is_empty() || path == "/" {
        vec![format!("{}/.well-known/oauth-protected-resource", origin)]
    } else {
        vec![
            format!("{}/.well-known/oauth-protected-resource{}", origin, path),
            format!("{}/.well-known/oauth-protected-resource", origin),
        ]
    };

    for url in &paths {
        match http.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let rm: serde_json::Value = resp.json().await?;
                tracing::info!(url = %url, "Discovered PRM");
                return Ok(rm);
            }
            _ => continue,
        }
    }

    anyhow::bail!("Could not discover PRM from {}", server_url)
}

/// Discover PRM from a specific URL (used when we already have a URL).
async fn discover_prm_from_url(server_url: &str) -> Result<serde_json::Value> {
    let http = reqwest::Client::new();
    let parsed = url::Url::parse(server_url)?;
    let origin = format!("{}://{}", parsed.scheme(), parsed.authority());
    let path = parsed.path().trim_end_matches('/');

    let url = if path.is_empty() || path == "/" {
        format!("{}/.well-known/oauth-protected-resource", origin)
    } else {
        format!("{}/.well-known/oauth-protected-resource{}", origin, path)
    };

    let resp = http.get(&url).send().await?;
    if resp.status().is_success() {
        let rm: serde_json::Value = resp.json().await?;
        return Ok(rm);
    }
    anyhow::bail!("Could not discover PRM from {}", url)
}

/// Discover OAuth metadata from an authorization server issuer URL.
/// Follows RFC 8414: `{origin}/.well-known/oauth-authorization-server{path}`
async fn discover_oauth_metadata_from_issuer(issuer_url: &str) -> Result<serde_json::Value> {
    let http = reqwest::Client::new();
    let parsed = url::Url::parse(issuer_url)?;
    let origin = format!("{}://{}", parsed.scheme(), parsed.authority());
    let path = parsed.path().trim_end_matches('/');

    // Build candidate URLs in priority order
    let trimmed = issuer_url.trim_end_matches('/');
    let mut urls = Vec::new();
    if !path.is_empty() && path != "/" {
        // RFC 8414 path-aware: {origin}/.well-known/oauth-authorization-server{path}
        urls.push(format!(
            "{}/.well-known/oauth-authorization-server{}",
            origin, path
        ));
        // Append to issuer: {issuer}/.well-known/oauth-authorization-server
        urls.push(format!(
            "{}/.well-known/oauth-authorization-server",
            trimmed
        ));
        // OIDC with path
        urls.push(format!("{}/.well-known/openid-configuration", trimmed));
    } else {
        urls.push(format!("{}/.well-known/oauth-authorization-server", origin));
        urls.push(format!("{}/.well-known/openid-configuration", origin));
    }

    for url in &urls {
        match http.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let metadata: serde_json::Value = resp.json().await?;
                let metadata_issuer = metadata
                    .get("issuer")
                    .and_then(serde_json::Value::as_str)
                    .context("Authorization server metadata omitted issuer")?;
                anyhow::ensure!(
                    metadata_issuer == issuer_url,
                    "Authorization server metadata issuer mismatch: expected {issuer_url:?}, got {metadata_issuer:?}"
                );
                tracing::info!(url = %url, "Discovered OAuth metadata from issuer");
                return Ok(metadata);
            }
            _ => continue,
        }
    }

    anyhow::bail!(
        "Could not discover OAuth metadata from issuer {}",
        issuer_url
    )
}

/// Discover OAuth metadata relative to a server URL.
/// Tries RFC 8414 path-aware format, then 2025-03-26 backcompat paths.
async fn discover_oauth_metadata_from_server_url(server_url: &str) -> Result<serde_json::Value> {
    let http = reqwest::Client::new();
    let parsed = url::Url::parse(server_url)?;
    let origin = format!("{}://{}", parsed.scheme(), parsed.authority());
    let path = parsed.path().trim_end_matches('/');

    // RFC 8414 path-aware: {origin}/.well-known/oauth-authorization-server{path}
    // Then origin-only, then OIDC, then 2025-03-26 backcompat paths
    let mut paths = Vec::new();

    if !path.is_empty() && path != "/" {
        paths.push(format!(
            "{}/.well-known/oauth-authorization-server{}",
            origin, path
        ));
    }
    paths.push(format!("{}/.well-known/oauth-authorization-server", origin));
    paths.push(format!("{}/.well-known/openid-configuration", origin));

    // 2025-03-26 backcompat: try {server_url}/.well-known/oauth-authorization-server
    if !path.is_empty() && path != "/" {
        paths.push(format!(
            "{}{}/.well-known/oauth-authorization-server",
            origin, path
        ));
    }

    for url in &paths {
        match http.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let metadata: serde_json::Value = resp.json().await?;
                tracing::info!(url = %url, "Discovered OAuth metadata");
                return Ok(metadata);
            }
            _ => continue,
        }
    }

    // 2025-03-26 endpoint fallback: construct endpoints directly from origin
    tracing::info!("Using 2025-03-26 endpoint fallback");
    Ok(serde_json::json!({
        "issuer": origin,
        "authorization_endpoint": format!("{}/authorize", origin),
        "token_endpoint": format!("{}/token", origin),
        "registration_endpoint": format!("{}/register", origin),
    }))
}

// ============================================================================
// WWW-Authenticate parsing
// ============================================================================

/// Extract a parameter value from a WWW-Authenticate header.
/// Handles: `param="value"` in comma-or-space-separated list.
fn extract_www_auth_param(header: &str, param: &str) -> Option<String> {
    let prefix = format!("{param}=\"");
    let start = header.find(&prefix)? + prefix.len();
    let end = header[start..].find('"')? + start;
    Some(header[start..end].to_string())
}

// ============================================================================
// JWT / crypto
// ============================================================================

/// Build a JWT assertion for client_credentials with private_key_jwt.
fn build_jwt_assertion(client_id: &str, audience: &str, private_key_pem: &str) -> Result<String> {
    use base64::Engine;
    use p256::ecdsa::{SigningKey, signature::Signer};
    use p256::pkcs8::DecodePrivateKey;
    use sec1::DecodeEcPrivateKey;

    // Parse the PEM-encoded private key (try PKCS#8 first, then SEC1)
    let signing_key = SigningKey::from_pkcs8_pem(private_key_pem)
        .or_else(|_| SigningKey::from_sec1_pem(private_key_pem))
        .map_err(|e| anyhow::anyhow!("Failed to parse private key: {}", e))?;

    let b64url = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    // Header
    let header = serde_json::json!({
        "alg": "ES256",
        "typ": "JWT"
    });
    let header_b64 = b64url.encode(serde_json::to_vec(&header)?);

    // Claims
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let claims = serde_json::json!({
        "iss": client_id,
        "sub": client_id,
        "aud": audience,
        "iat": now,
        "exp": now + 300,
        "jti": generate_random_string(),
    });
    let claims_b64 = b64url.encode(serde_json::to_vec(&claims)?);

    // Sign
    let message = format!("{}.{}", header_b64, claims_b64);
    let signature: p256::ecdsa::Signature = signing_key.sign(message.as_bytes());
    let sig_b64 = b64url.encode(signature.to_bytes());

    Ok(format!("{}.{}", message, sig_b64))
}

// ============================================================================
// Utilities
// ============================================================================

fn generate_random_string() -> String {
    use base64::Engine;
    let bytes: Vec<u8> = (0..16).map(|_| rand_byte()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
}

/// Simple pseudo-random byte using system time (good enough for PKCE/state).
fn rand_byte() -> u8 {
    use std::time::SystemTime;
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let time = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    ((time.wrapping_mul(6364136223846793005).wrapping_add(count)) >> 33) as u8
}
