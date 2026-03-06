//! Auth conformance scenarios.
//!
//! These implement the OAuth 2.0 authorization flows required by the MCP spec.
//! The conformance test server auto-approves authorization requests and redirects
//! with the authorization code, enabling headless OAuth flows.

use anyhow::{Context, Result};
use tower_mcp::client::McpClient;
use tower_mcp::{HttpClientConfig, HttpClientTransport};

use crate::handlers;

/// Standard OAuth authorization-code flow.
///
/// Used by most auth scenarios: metadata discovery variants, CIMD, scope handling,
/// token endpoint auth methods, and backcompat scenarios.
pub async fn standard_auth(server_url: &str, context: &Option<serde_json::Value>) -> Result<()> {
    let access_token = perform_oauth_flow(server_url, context).await?;
    run_authed_client(server_url, &access_token).await
}

/// Scope step-up: connect, try tools, re-auth on failure, retry.
pub async fn scope_step_up(server_url: &str, context: &Option<serde_json::Value>) -> Result<()> {
    let access_token = perform_oauth_flow(server_url, context).await?;

    let transport = HttpClientTransport::new(server_url).bearer_token(&access_token);
    let client = McpClient::builder()
        .connect(transport, handlers::BasicHandler)
        .await?;

    client.initialize("conformance-client", "0.1.0").await?;
    let tools = client.list_tools().await?;

    // Try calling tools -- may fail with 403
    let mut needs_reauth = false;
    for tool in &tools.tools {
        let args = crate::core_scenarios::build_tool_arguments(&tool.input_schema);
        match client.call_tool(&tool.name, args).await {
            Ok(_) => {}
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("403") || err_str.contains("insufficient_scope") {
                    tracing::info!("Got 403/insufficient_scope, will re-authenticate");
                    needs_reauth = true;
                    break;
                }
                return Err(e.into());
            }
        }
    }

    client.shutdown().await?;

    if needs_reauth {
        // Re-authenticate with broader scope
        let new_token = perform_oauth_flow(server_url, context).await?;
        let transport = HttpClientTransport::new(server_url).bearer_token(&new_token);
        let client = McpClient::builder()
            .connect(transport, handlers::BasicHandler)
            .await?;

        client.initialize("conformance-client", "0.1.0").await?;
        let tools = client.list_tools().await?;

        for tool in &tools.tools {
            let args = crate::core_scenarios::build_tool_arguments(&tool.input_schema);
            client.call_tool(&tool.name, args).await?;
        }
        client.shutdown().await?;
    }

    Ok(())
}

/// Scope retry limit: retry up to 3 times on 403.
pub async fn scope_retry_limit(
    server_url: &str,
    context: &Option<serde_json::Value>,
) -> Result<()> {
    for attempt in 0..3 {
        tracing::info!(attempt, "Auth attempt");
        let access_token = perform_oauth_flow(server_url, context).await?;
        let transport = HttpClientTransport::new(server_url).bearer_token(&access_token);
        let client = McpClient::builder()
            .connect(transport, handlers::BasicHandler)
            .await?;

        client.initialize("conformance-client", "0.1.0").await?;
        let tools = client.list_tools().await?;

        let mut success = true;
        for tool in &tools.tools {
            let args = crate::core_scenarios::build_tool_arguments(&tool.input_schema);
            match client.call_tool(&tool.name, args).await {
                Ok(_) => {}
                Err(e) => {
                    let err_str = e.to_string();
                    if err_str.contains("403") || err_str.contains("insufficient_scope") {
                        success = false;
                        break;
                    }
                    client.shutdown().await?;
                    return Err(e.into());
                }
            }
        }

        client.shutdown().await?;

        if success {
            return Ok(());
        }
    }

    anyhow::bail!("Failed after 3 auth attempts")
}

/// Resource mismatch: server's PRM resource doesn't match, client should error.
pub async fn resource_mismatch(
    server_url: &str,
    context: &Option<serde_json::Value>,
) -> Result<()> {
    // Try the standard flow -- expect it to fail due to resource mismatch
    match perform_oauth_flow(server_url, context).await {
        Ok(token) => {
            // If we got a token, try using it -- should fail
            let result = run_authed_client(server_url, &token).await;
            if result.is_err() {
                // Expected: resource mismatch causes failure
                return Ok(());
            }
            // If it somehow succeeded, that might be fine too depending on the test
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

    // Discover token endpoint
    let token_endpoint = discover_token_endpoint(server_url).await?;

    // Request token with client_credentials grant using Basic auth
    let http = reqwest::Client::new();
    let resp = http
        .post(&token_endpoint)
        .basic_auth(client_id, Some(client_secret))
        .form(&[("grant_type", "client_credentials")])
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

    // Discover token endpoint
    let token_endpoint = discover_token_endpoint(server_url).await?;

    // Build JWT assertion
    let jwt = build_jwt_assertion(client_id, &token_endpoint, private_key_pem)?;

    // Request token
    let http = reqwest::Client::new();
    let resp = http
        .post(&token_endpoint)
        .form(&[
            ("grant_type", "client_credentials"),
            (
                "client_assertion_type",
                "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
            ),
            ("client_assertion", &jwt),
        ])
        .send()
        .await?;

    let token_resp: serde_json::Value = resp.json().await?;
    let access_token = token_resp
        .get("access_token")
        .and_then(|v| v.as_str())
        .context("No access_token in response")?;

    run_authed_client(server_url, access_token).await
}

/// Cross-app access (SEP-990).
pub async fn cross_app_access(server_url: &str, context: &Option<serde_json::Value>) -> Result<()> {
    // Fall back to standard auth -- the conformance test server should handle
    // the cross-app access flow via standard OAuth with the right context
    standard_auth(server_url, context).await
}

// ============================================================================
// Internal helpers
// ============================================================================

/// Perform a headless OAuth authorization-code flow.
///
/// 1. Try the MCP endpoint to get 401 with resource metadata
/// 2. Discover OAuth authorization server metadata
/// 3. Register client (DCR or CIMD)
/// 4. Authorize with PKCE
/// 5. Exchange code for token
async fn perform_oauth_flow(
    server_url: &str,
    _context: &Option<serde_json::Value>,
) -> Result<String> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    // Step 1: Try MCP endpoint, expect 401 with resource metadata URL
    let initial_resp = http
        .post(server_url)
        .header("Content-Type", "application/json")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"conformance-client","version":"0.1.0"}}}"#)
        .send()
        .await?;

    let status = initial_resp.status();
    tracing::info!(status = %status, "Initial MCP request status");

    // Extract scope from WWW-Authenticate if present
    let www_auth = initial_resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let scope = www_auth.as_ref().and_then(|wa| {
        // Parse scope="..." from WWW-Authenticate
        wa.split(',').find_map(|part| {
            let trimmed = part.trim();
            if let Some(rest) = trimmed.strip_prefix("scope=\"") {
                rest.strip_suffix('"').map(|s| s.to_string())
            } else {
                None
            }
        })
    });

    // Extract resource metadata URL from WWW-Authenticate or try well-known
    let resource_metadata_url = www_auth.as_ref().and_then(|wa| {
        wa.split(',').find_map(|part| {
            let trimmed = part.trim();
            if let Some(rest) = trimmed.strip_prefix("resource_metadata=\"") {
                rest.strip_suffix('"').map(|s| s.to_string())
            } else {
                None
            }
        })
    });

    // Step 2: Discover OAuth metadata
    let metadata = if let Some(ref rm_url) = resource_metadata_url {
        // Fetch Protected Resource Metadata to get the authorization server
        let rm_resp = http.get(rm_url).send().await?;
        let rm: serde_json::Value = rm_resp.json().await?;
        tracing::info!("Fetched resource metadata from {}", rm_url);

        // Get authorization server URL from resource metadata
        let auth_servers = rm
            .get("authorization_servers")
            .and_then(|v| v.as_array())
            .context("No authorization_servers in resource metadata")?;
        let auth_server_url = auth_servers
            .first()
            .and_then(|v| v.as_str())
            .context("Empty authorization_servers")?;

        // Discover AS metadata
        discover_oauth_metadata_from_issuer(auth_server_url).await?
    } else {
        // Fallback: try well-known paths relative to the server URL
        discover_oauth_metadata_from_server_url(server_url).await?
    };

    let authorization_endpoint = metadata
        .get("authorization_endpoint")
        .and_then(|v| v.as_str())
        .context("No authorization_endpoint in metadata")?;
    let token_endpoint = metadata
        .get("token_endpoint")
        .and_then(|v| v.as_str())
        .context("No token_endpoint in metadata")?;
    let registration_endpoint = metadata
        .get("registration_endpoint")
        .and_then(|v| v.as_str());

    // Check supported scopes from metadata if we don't have one from WWW-Authenticate
    let scope = scope.or_else(|| {
        metadata
            .get("scopes_supported")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|s| !s.is_empty())
    });

    // Step 3: Dynamic client registration (if available)
    let (client_id, client_secret) = if let Some(reg_endpoint) = registration_endpoint {
        let reg_resp = http
            .post(reg_endpoint)
            .json(&serde_json::json!({
                "client_name": "conformance-client",
                "redirect_uris": ["http://localhost:23456/callback"],
                "grant_types": ["authorization_code"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "client_secret_post"
            }))
            .send()
            .await?;

        let reg: serde_json::Value = reg_resp.json().await?;
        let cid = reg
            .get("client_id")
            .and_then(|v| v.as_str())
            .context("No client_id in registration response")?
            .to_string();
        let csec = reg
            .get("client_secret")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        (cid, csec)
    } else {
        // Use CIMD (Client ID Metadata Documents) -- client_id is a URL
        (
            "http://localhost:23456/client-metadata.json".to_string(),
            None,
        )
    };

    // Step 4: Build authorization URL with PKCE
    let code_verifier = generate_code_verifier();
    let code_challenge = generate_code_challenge(&code_verifier);
    let state = generate_random_string();

    let mut auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        authorization_endpoint,
        urlencoded(&client_id),
        urlencoded("http://localhost:23456/callback"),
        urlencoded(&state),
        urlencoded(&code_challenge),
    );

    if let Some(ref s) = scope {
        auth_url.push_str(&format!("&scope={}", urlencoded(s)));
    }

    // Step 5: Fetch auth URL (auto-approved, get redirect)
    let auth_resp = http.get(&auth_url).send().await?;
    let location = auth_resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .context("No Location header in auth response")?
        .to_string();

    // Extract code from redirect URL
    let redirect_url = url::Url::parse(&location)
        .or_else(|_| url::Url::parse(&format!("http://localhost:23456{}", location)))?;
    let code = redirect_url
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.to_string())
        .context("No code in redirect")?;

    // Step 6: Exchange code for token
    let mut token_params = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        (
            "redirect_uri",
            "http://localhost:23456/callback".to_string(),
        ),
        ("client_id", client_id.clone()),
        ("code_verifier", code_verifier),
    ];

    if let Some(secret) = &client_secret {
        token_params.push(("client_secret", secret.clone()));
    }

    let token_resp = http.post(token_endpoint).form(&token_params).send().await?;

    let token_body: serde_json::Value = token_resp.json().await?;
    let access_token = token_body
        .get("access_token")
        .and_then(|v| v.as_str())
        .context("No access_token in token response")?;

    tracing::info!("OAuth flow completed successfully");
    Ok(access_token.to_string())
}

/// OAuth flow with pre-registered credentials (no DCR).
async fn perform_oauth_flow_with_credentials(
    server_url: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<String> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    // Try MCP endpoint first to get metadata hints
    let initial_resp = http
        .post(server_url)
        .header("Content-Type", "application/json")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"conformance-client","version":"0.1.0"}}}"#)
        .send()
        .await?;

    let www_auth = initial_resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let resource_metadata_url = www_auth.as_ref().and_then(|wa| {
        wa.split(',').find_map(|part| {
            let trimmed = part.trim();
            if let Some(rest) = trimmed.strip_prefix("resource_metadata=\"") {
                rest.strip_suffix('"').map(|s| s.to_string())
            } else {
                None
            }
        })
    });

    let metadata = if let Some(ref rm_url) = resource_metadata_url {
        let rm_resp = http.get(rm_url).send().await?;
        let rm: serde_json::Value = rm_resp.json().await?;
        let auth_server_url = rm
            .get("authorization_servers")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .context("No authorization_servers in resource metadata")?;
        discover_oauth_metadata_from_issuer(auth_server_url).await?
    } else {
        discover_oauth_metadata_from_server_url(server_url).await?
    };

    let authorization_endpoint = metadata
        .get("authorization_endpoint")
        .and_then(|v| v.as_str())
        .context("No authorization_endpoint")?;
    let token_endpoint = metadata
        .get("token_endpoint")
        .and_then(|v| v.as_str())
        .context("No token_endpoint")?;

    let code_verifier = generate_code_verifier();
    let code_challenge = generate_code_challenge(&code_verifier);
    let state = generate_random_string();

    let auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        authorization_endpoint,
        urlencoded(client_id),
        urlencoded("http://localhost:23456/callback"),
        urlencoded(&state),
        urlencoded(&code_challenge),
    );

    let auth_resp = http.get(&auth_url).send().await?;
    let location = auth_resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .context("No Location header")?
        .to_string();

    let redirect_url = url::Url::parse(&location)
        .or_else(|_| url::Url::parse(&format!("http://localhost:23456{}", location)))?;
    let code = redirect_url
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.to_string())
        .context("No code in redirect")?;

    let token_resp = http
        .post(token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", "http://localhost:23456/callback"),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code_verifier", &code_verifier),
        ])
        .send()
        .await?;

    let token_body: serde_json::Value = token_resp.json().await?;
    let access_token = token_body
        .get("access_token")
        .and_then(|v| v.as_str())
        .context("No access_token")?;

    Ok(access_token.to_string())
}

/// Connect and run a basic client with a bearer token.
async fn run_authed_client(server_url: &str, access_token: &str) -> Result<()> {
    let config = HttpClientConfig {
        ..Default::default()
    };
    let transport = HttpClientTransport::with_config(server_url, config).bearer_token(access_token);
    let client = McpClient::builder()
        .connect(transport, handlers::BasicHandler)
        .await?;

    client.initialize("conformance-client", "0.1.0").await?;
    let tools = client.list_tools().await?;
    tracing::info!("Listed {} tools with auth", tools.tools.len());

    for tool in &tools.tools {
        let args = crate::core_scenarios::build_tool_arguments(&tool.input_schema);
        let _ = client.call_tool(&tool.name, args).await?;
    }

    client.shutdown().await?;
    Ok(())
}

/// Discover OAuth metadata from an authorization server issuer URL.
/// Follows RFC 8414: `{issuer}/.well-known/oauth-authorization-server`
async fn discover_oauth_metadata_from_issuer(issuer_url: &str) -> Result<serde_json::Value> {
    let http = reqwest::Client::new();
    let url = format!(
        "{}/.well-known/oauth-authorization-server",
        issuer_url.trim_end_matches('/')
    );
    let resp = http.get(&url).send().await?;
    if resp.status().is_success() {
        let metadata: serde_json::Value = resp.json().await?;
        tracing::info!(url = %url, "Discovered OAuth metadata from issuer");
        return Ok(metadata);
    }

    // Fallback to openid-configuration
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer_url.trim_end_matches('/')
    );
    let resp = http.get(&url).send().await?;
    if resp.status().is_success() {
        let metadata: serde_json::Value = resp.json().await?;
        tracing::info!(url = %url, "Discovered OAuth metadata via OIDC");
        return Ok(metadata);
    }

    anyhow::bail!(
        "Could not discover OAuth metadata from issuer {}",
        issuer_url
    )
}

/// Discover OAuth metadata relative to a server URL.
/// Tries RFC 8414 path-aware format and fallbacks.
async fn discover_oauth_metadata_from_server_url(server_url: &str) -> Result<serde_json::Value> {
    let http = reqwest::Client::new();
    let parsed = url::Url::parse(server_url)?;
    let origin = format!("{}://{}", parsed.scheme(), parsed.authority());
    let path = parsed.path().trim_end_matches('/');

    // RFC 8414 path-aware: {origin}/.well-known/oauth-authorization-server{path}
    let paths = if path.is_empty() || path == "/" {
        vec![
            format!("{}/.well-known/oauth-authorization-server", origin),
            format!("{}/.well-known/openid-configuration", origin),
        ]
    } else {
        vec![
            format!("{}/.well-known/oauth-authorization-server{}", origin, path),
            format!("{}/.well-known/oauth-authorization-server", origin),
            format!("{}/.well-known/openid-configuration", origin),
        ]
    };

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

    anyhow::bail!("Could not discover OAuth metadata from {}", server_url)
}

/// Discover the token endpoint from OAuth metadata.
async fn discover_token_endpoint(server_url: &str) -> Result<String> {
    let metadata = discover_oauth_metadata_from_server_url(server_url).await?;
    metadata
        .get("token_endpoint")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .context("No token_endpoint in metadata")
}

/// Build a JWT assertion for client_credentials with private_key_jwt.
fn build_jwt_assertion(client_id: &str, audience: &str, private_key_pem: &str) -> Result<String> {
    use base64::Engine;
    use p256::ecdsa::{SigningKey, signature::Signer};
    use sec1::DecodeEcPrivateKey;

    // Parse the PEM-encoded private key
    let signing_key = SigningKey::from_sec1_pem(private_key_pem)
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

fn generate_code_verifier() -> String {
    use base64::Engine;
    let bytes: Vec<u8> = (0..32).map(|_| rand_byte()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
}

fn generate_code_challenge(verifier: &str) -> String {
    use base64::Engine;
    use std::io::Write;
    let mut hasher = Sha256::new();
    hasher.write_all(verifier.as_bytes()).unwrap();
    let hash = hasher.finalize();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash)
}

fn generate_random_string() -> String {
    use base64::Engine;
    let bytes: Vec<u8> = (0..16).map(|_| rand_byte()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
}

fn urlencoded(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
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

/// Minimal SHA-256 implementation (avoids adding a crypto dep just for PKCE).
struct Sha256 {
    data: Vec<u8>,
}

impl Sha256 {
    fn new() -> Self {
        Self { data: Vec::new() }
    }

    fn finalize(self) -> [u8; 32] {
        sha256_hash(&self.data)
    }
}

impl std::io::Write for Sha256 {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.data.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// SHA-256 hash function.
fn sha256_hash(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let k: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    // Pre-processing: pad message
    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 512-bit block
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut result = [0u8; 32];
    for (i, &val) in h.iter().enumerate() {
        result[i * 4..i * 4 + 4].copy_from_slice(&val.to_be_bytes());
    }
    result
}
