//! OAuth 2.0 Authorization Code grant with PKCE for interactive authentication.
//!
//! Provides [`OAuthAuthorizationCode`] for acquiring access tokens via a
//! browser-based login flow. The flow:
//!
//! 1. Discover the authorization server metadata (RFC 8414)
//! 2. Generate a PKCE code verifier and challenge (RFC 7636)
//! 3. Redirect the user to the authorization endpoint
//! 4. Receive the authorization code via a local callback server
//! 5. Exchange the code for tokens at the token endpoint
//! 6. Cache and automatically refresh tokens before expiry
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::client::OAuthAuthorizationCode;
//!
//! # async fn example() -> Result<(), tower_mcp::BoxError> {
//! let provider = OAuthAuthorizationCode::start(
//!     "https://mcp.example.com",
//!     &["mcp:tools", "mcp:resources"],
//! ).await?;
//!
//! // Open the authorization URL in the user's browser
//! println!("Open: {}", provider.authorization_url());
//!
//! // Wait for the callback (blocks until user completes login)
//! provider.wait_for_callback().await?;
//!
//! // Now use as a TokenProvider
//! let transport = tower_mcp::client::HttpClientTransport::new("https://mcp.example.com")
//!     .with_token_provider(provider);
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::{Mutex, RwLock, oneshot};

use super::oauth::{OAuthClientError, TokenProvider};

// =============================================================================
// PKCE (RFC 7636)
// =============================================================================

/// Generate a cryptographically random code verifier (43-128 chars, unreserved).
fn generate_code_verifier() -> String {
    use base64::Engine;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("getrandom failed");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Compute the S256 code challenge from a code verifier.
fn compute_code_challenge(verifier: &str) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash)
}

/// Generate a random CSRF state parameter.
fn generate_state() -> String {
    use base64::Engine;
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("getrandom failed");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// =============================================================================
// Authorization Server Discovery (RFC 8414)
// =============================================================================

/// OAuth authorization server metadata (subset of RFC 8414).
#[derive(Debug, serde::Deserialize)]
struct AuthorizationServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    #[allow(dead_code)]
    registration_endpoint: Option<String>,
}

/// Discover the authorization server metadata from the MCP server's
/// Protected Resource Metadata (RFC 9728) or directly from well-known.
async fn discover_auth_server(
    server_url: &str,
    client: &reqwest::Client,
) -> Result<AuthorizationServerMetadata, OAuthClientError> {
    let base = server_url.trim_end_matches('/');

    // Try Protected Resource Metadata first (RFC 9728)
    if let Some(metadata) = try_discover_via_prm(base, client).await {
        return Ok(metadata);
    }

    // Fallback: try well-known directly on the server URL
    let meta_url = format!("{}/.well-known/oauth-authorization-server", base);
    client
        .get(&meta_url)
        .send()
        .await
        .map_err(|e| OAuthClientError::Discovery(e.to_string()))?
        .json()
        .await
        .map_err(|e| OAuthClientError::Discovery(e.to_string()))
}

/// Try to discover auth server via Protected Resource Metadata (RFC 9728).
async fn try_discover_via_prm(
    base: &str,
    client: &reqwest::Client,
) -> Option<AuthorizationServerMetadata> {
    let prm_url = format!("{}/.well-known/oauth-protected-resource", base);
    let resp = client.get(&prm_url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let prm: serde_json::Value = resp.json().await.ok()?;
    let auth_server = prm["authorization_servers"].as_array()?.first()?.as_str()?;
    let meta_url = format!(
        "{}/.well-known/oauth-authorization-server",
        auth_server.trim_end_matches('/')
    );
    let meta = client
        .get(&meta_url)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    meta.json().await.ok()
}

// =============================================================================
// Token types
// =============================================================================

/// Token response from the authorization server.
#[derive(Debug, Clone, serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    #[allow(dead_code)]
    token_type: String,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    #[allow(dead_code)]
    scope: Option<String>,
}

/// Cached token with expiry and optional refresh token.
#[derive(Debug, Clone)]
struct CachedAuthCodeToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Instant,
}

// =============================================================================
// OAuthAuthorizationCode
// =============================================================================

/// OAuth 2.0 Authorization Code token provider with PKCE.
///
/// Handles the interactive browser-based login flow and provides
/// automatic token caching and refresh.
#[derive(Clone)]
pub struct OAuthAuthorizationCode {
    inner: Arc<OAuthAuthCodeInner>,
}

struct OAuthAuthCodeInner {
    /// The authorization URL the user should open in their browser.
    authorization_url: String,
    /// Token endpoint for code exchange and refresh.
    token_endpoint: String,
    /// Client ID (from dynamic registration or configuration).
    client_id: String,
    /// Client secret (if provided by registration).
    client_secret: Option<String>,
    /// PKCE code verifier (sent during token exchange).
    code_verifier: String,
    /// CSRF state parameter for validation.
    state: String,
    /// Redirect URI used for the callback.
    redirect_uri: String,
    /// Scopes requested.
    scopes: Option<String>,
    /// Refresh buffer before expiry.
    refresh_buffer: Duration,
    /// HTTP client.
    client: reqwest::Client,
    /// Cached token.
    cache: RwLock<Option<CachedAuthCodeToken>>,
    /// Callback receiver (consumed once).
    callback_rx: Mutex<Option<oneshot::Receiver<Result<CallbackResult, String>>>>,
    /// Handle to the callback server task.
    _callback_task: tokio::task::JoinHandle<()>,
}

#[derive(Debug)]
struct CallbackResult {
    code: String,
    #[allow(dead_code)]
    state: String,
}

impl fmt::Debug for OAuthAuthorizationCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthAuthorizationCode")
            .field("client_id", &self.inner.client_id)
            .field("token_endpoint", &self.inner.token_endpoint)
            .field("redirect_uri", &self.inner.redirect_uri)
            .finish()
    }
}

impl OAuthAuthorizationCode {
    /// Start an OAuth Authorization Code flow.
    ///
    /// Discovers the authorization server, generates PKCE parameters,
    /// starts a local callback server, and returns a provider ready for
    /// the user to authorize.
    ///
    /// After calling this, open [`authorization_url()`](Self::authorization_url)
    /// in the user's browser, then call [`wait_for_callback()`](Self::wait_for_callback).
    pub async fn start(server_url: &str, scopes: &[&str]) -> Result<Self, OAuthClientError> {
        Self::start_with_config(server_url, scopes, OAuthAuthCodeConfig::default()).await
    }

    /// Start with custom configuration.
    pub async fn start_with_config(
        server_url: &str,
        scopes: &[&str],
        config: OAuthAuthCodeConfig,
    ) -> Result<Self, OAuthClientError> {
        let client = config.http_client.unwrap_or_default();

        // Discover auth server
        let metadata = discover_auth_server(server_url, &client).await?;

        // Generate PKCE
        let code_verifier = generate_code_verifier();
        let code_challenge = compute_code_challenge(&code_verifier);
        let state = generate_state();

        // Start callback server
        let callback_port = config.callback_port.unwrap_or(0);
        let (callback_tx, callback_rx) = oneshot::channel();
        let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", callback_port))
            .await
            .map_err(|e| OAuthClientError::BuildError(format!("Callback server bind: {}", e)))?;
        let actual_port = listener
            .local_addr()
            .map_err(|e| OAuthClientError::BuildError(format!("Get local addr: {}", e)))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{}/callback", actual_port);

        let expected_state = state.clone();
        let callback_task = tokio::spawn(async move {
            run_callback_server(listener, callback_tx, expected_state).await;
        });

        // Build authorization URL
        let scope_str = if scopes.is_empty() {
            None
        } else {
            Some(scopes.join(" "))
        };

        let mut auth_url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
            metadata.authorization_endpoint,
            urlencoding::encode(config.client_id.as_deref().unwrap_or("tower-mcp")),
            urlencoding::encode(&redirect_uri),
            urlencoding::encode(&state),
            urlencoding::encode(&code_challenge),
        );
        if let Some(ref s) = scope_str {
            auth_url.push_str("&scope=");
            auth_url.push_str(&urlencoding::encode(s));
        }

        let client_id = config.client_id.unwrap_or_else(|| "tower-mcp".to_string());

        Ok(Self {
            inner: Arc::new(OAuthAuthCodeInner {
                authorization_url: auth_url,
                token_endpoint: metadata.token_endpoint,
                client_id,
                client_secret: config.client_secret,
                code_verifier,
                state,
                redirect_uri,
                scopes: scope_str,
                refresh_buffer: config.refresh_buffer,
                client,
                cache: RwLock::new(None),
                callback_rx: Mutex::new(Some(callback_rx)),
                _callback_task: callback_task,
            }),
        })
    }

    /// Get the authorization URL to open in the user's browser.
    pub fn authorization_url(&self) -> &str {
        &self.inner.authorization_url
    }

    /// Wait for the OAuth callback and exchange the authorization code for tokens.
    ///
    /// This blocks until the user completes the browser-based authorization
    /// or the callback times out.
    pub async fn wait_for_callback(&self) -> Result<(), OAuthClientError> {
        self.wait_for_callback_with_timeout(Duration::from_secs(300))
            .await
    }

    /// Wait for callback with a custom timeout.
    pub async fn wait_for_callback_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<(), OAuthClientError> {
        let rx = self.inner.callback_rx.lock().await.take().ok_or_else(|| {
            OAuthClientError::InvalidResponse("Callback already consumed".to_string())
        })?;

        let result = tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| {
                OAuthClientError::TokenRequest("Timed out waiting for OAuth callback".to_string())
            })?
            .map_err(|_| OAuthClientError::TokenRequest("Callback cancelled".to_string()))?
            .map_err(|e| OAuthClientError::TokenRequest(format!("Callback error: {}", e)))?;

        // Validate CSRF state
        if result.state != self.inner.state {
            return Err(OAuthClientError::InvalidResponse(
                "CSRF state mismatch".to_string(),
            ));
        }

        // Exchange code for tokens
        let token = self.exchange_code(&result.code).await?;
        *self.inner.cache.write().await = Some(token);

        Ok(())
    }

    /// Exchange an authorization code for tokens.
    async fn exchange_code(&self, code: &str) -> Result<CachedAuthCodeToken, OAuthClientError> {
        let mut body = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&code_verifier={}&client_id={}",
            urlencoding::encode(code),
            urlencoding::encode(&self.inner.redirect_uri),
            urlencoding::encode(&self.inner.code_verifier),
            urlencoding::encode(&self.inner.client_id),
        );

        if let Some(ref secret) = self.inner.client_secret {
            body.push_str("&client_secret=");
            body.push_str(&urlencoding::encode(secret));
        }

        let response = self
            .inner
            .client
            .post(&self.inner.token_endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| OAuthClientError::TokenRequest(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(OAuthClientError::TokenRequest(format!(
                "HTTP {}: {}",
                status, body
            )));
        }

        let token_response: TokenResponse = response
            .json()
            .await
            .map_err(|e| OAuthClientError::InvalidResponse(e.to_string()))?;

        Ok(to_cached_token(token_response))
    }

    /// Refresh the access token using the refresh token.
    async fn refresh_token(
        &self,
        refresh_token: &str,
    ) -> Result<CachedAuthCodeToken, OAuthClientError> {
        let mut body = format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}",
            urlencoding::encode(refresh_token),
            urlencoding::encode(&self.inner.client_id),
        );

        if let Some(ref secret) = self.inner.client_secret {
            body.push_str("&client_secret=");
            body.push_str(&urlencoding::encode(secret));
        }

        if let Some(ref scopes) = self.inner.scopes {
            body.push_str("&scope=");
            body.push_str(&urlencoding::encode(scopes));
        }

        let response = self
            .inner
            .client
            .post(&self.inner.token_endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| OAuthClientError::TokenRequest(format!("Refresh failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(OAuthClientError::TokenRequest(format!(
                "Refresh HTTP {}: {}",
                status, body
            )));
        }

        let mut token_response: TokenResponse = response
            .json()
            .await
            .map_err(|e| OAuthClientError::InvalidResponse(e.to_string()))?;

        // Preserve the refresh token if the server doesn't return a new one
        if token_response.refresh_token.is_none() {
            token_response.refresh_token = Some(refresh_token.to_string());
        }

        Ok(to_cached_token(token_response))
    }
}

fn to_cached_token(response: TokenResponse) -> CachedAuthCodeToken {
    let expires_in = Duration::from_secs(response.expires_in.unwrap_or(3600));
    CachedAuthCodeToken {
        access_token: response.access_token,
        refresh_token: response.refresh_token,
        expires_at: Instant::now() + expires_in,
    }
}

fn is_token_valid(token: &CachedAuthCodeToken, buffer: Duration) -> bool {
    token
        .expires_at
        .checked_sub(buffer)
        .is_some_and(|effective| Instant::now() < effective)
}

#[async_trait]
impl TokenProvider for OAuthAuthorizationCode {
    async fn get_token(&self) -> Result<String, OAuthClientError> {
        // Fast path: cached token is still valid
        {
            let cache = self.inner.cache.read().await;
            if let Some(ref token) = *cache
                && is_token_valid(token, self.inner.refresh_buffer)
            {
                return Ok(token.access_token.clone());
            }
        }

        // Slow path: refresh or fail
        let mut cache = self.inner.cache.write().await;

        // Double-check after acquiring write lock
        if let Some(ref token) = *cache
            && is_token_valid(token, self.inner.refresh_buffer)
        {
            return Ok(token.access_token.clone());
        }

        // Try refresh if we have a refresh token
        if let Some(ref token) = *cache
            && let Some(ref refresh) = token.refresh_token
        {
            tracing::debug!("Refreshing OAuth access token");
            match self.refresh_token(refresh).await {
                Ok(new_token) => {
                    let access = new_token.access_token.clone();
                    *cache = Some(new_token);
                    return Ok(access);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Token refresh failed");
                    // Fall through - caller will need to re-authenticate
                }
            }
        }

        Err(OAuthClientError::TokenRequest(
            "No valid token available. Call wait_for_callback() to authenticate.".to_string(),
        ))
    }
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for [`OAuthAuthorizationCode`].
pub struct OAuthAuthCodeConfig {
    /// OAuth client ID. Default: `"tower-mcp"`.
    pub client_id: Option<String>,
    /// OAuth client secret (if the server requires it).
    pub client_secret: Option<String>,
    /// Port for the local callback server. Default: random available port.
    pub callback_port: Option<u16>,
    /// Buffer before token expiry to trigger refresh. Default: 30 seconds.
    pub refresh_buffer: Duration,
    /// Custom reqwest client.
    pub http_client: Option<reqwest::Client>,
}

impl Default for OAuthAuthCodeConfig {
    fn default() -> Self {
        Self {
            client_id: None,
            client_secret: None,
            callback_port: None,
            refresh_buffer: Duration::from_secs(30),
            http_client: None,
        }
    }
}

// =============================================================================
// Callback Server
// =============================================================================

/// Run a minimal HTTP callback server for the OAuth redirect.
async fn run_callback_server(
    listener: tokio::net::TcpListener,
    tx: oneshot::Sender<Result<CallbackResult, String>>,
    expected_state: String,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut tx = Some(tx);

    // Accept one connection
    let Ok((mut stream, _)) = listener.accept().await else {
        if let Some(tx) = tx.take() {
            let _ = tx.send(Err("Callback server accept failed".to_string()));
        }
        return;
    };

    let mut buf = vec![0u8; 4096];
    let n = match stream.read(&mut buf).await {
        Ok(n) => n,
        Err(e) => {
            if let Some(tx) = tx.take() {
                let _ = tx.send(Err(format!("Read error: {}", e)));
            }
            return;
        }
    };

    let request = String::from_utf8_lossy(&buf[..n]);

    // Parse the GET request line to extract query parameters
    let result = if let Some(path) = request.lines().next().and_then(|line| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            Some(parts[1])
        } else {
            None
        }
    }) {
        parse_callback_query(path, &expected_state)
    } else {
        Err("Invalid HTTP request".to_string())
    };

    // Send response to browser
    let (status, body) = match &result {
        Ok(_) => (
            "200 OK",
            "Authorization successful. You can close this tab.",
        ),
        Err(e) => ("400 Bad Request", e.as_str()),
    };

    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;

    if let Some(tx) = tx.take() {
        let _ = tx.send(result);
    }
}

/// Parse the callback query string for code and state.
fn parse_callback_query(path: &str, expected_state: &str) -> Result<CallbackResult, String> {
    let query = path
        .split('?')
        .nth(1)
        .ok_or_else(|| "No query parameters in callback".to_string())?;

    let mut code = None;
    let mut state = None;
    let mut error = None;

    for param in query.split('&') {
        let mut parts = param.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let value = parts.next().unwrap_or("");
        let decoded = urlencoding::decode(value).unwrap_or_default().to_string();

        match key {
            "code" => code = Some(decoded),
            "state" => state = Some(decoded),
            "error" => error = Some(decoded),
            "error_description" if error.is_none() => error = Some(decoded),
            _ => {}
        }
    }

    if let Some(err) = error {
        return Err(format!("OAuth error: {}", err));
    }

    let code = code.ok_or_else(|| "Missing 'code' parameter".to_string())?;
    let state = state.ok_or_else(|| "Missing 'state' parameter".to_string())?;

    if state != expected_state {
        return Err("CSRF state mismatch".to_string());
    }

    Ok(CallbackResult { code, state })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pkce_code_verifier_length() {
        let verifier = generate_code_verifier();
        assert!(
            verifier.len() >= 43,
            "Verifier too short: {}",
            verifier.len()
        );
        assert!(
            verifier.len() <= 128,
            "Verifier too long: {}",
            verifier.len()
        );
    }

    #[test]
    fn test_pkce_code_challenge_deterministic() {
        let challenge1 = compute_code_challenge("test-verifier");
        let challenge2 = compute_code_challenge("test-verifier");
        assert_eq!(challenge1, challenge2);
    }

    #[test]
    fn test_pkce_code_challenge_differs_for_different_input() {
        let c1 = compute_code_challenge("verifier-a");
        let c2 = compute_code_challenge("verifier-b");
        assert_ne!(c1, c2);
    }

    #[test]
    fn test_state_generation_unique() {
        let s1 = generate_state();
        let s2 = generate_state();
        assert_ne!(s1, s2);
    }

    #[test]
    fn test_parse_callback_success() {
        let result = parse_callback_query("/callback?code=abc123&state=mystate", "mystate");
        let cb = result.unwrap();
        assert_eq!(cb.code, "abc123");
        assert_eq!(cb.state, "mystate");
    }

    #[test]
    fn test_parse_callback_state_mismatch() {
        let result = parse_callback_query("/callback?code=abc123&state=wrong", "expected");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("CSRF"));
    }

    #[test]
    fn test_parse_callback_error() {
        let result = parse_callback_query(
            "/callback?error=access_denied&error_description=User+denied+access",
            "state",
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("access_denied"));
    }

    #[test]
    fn test_parse_callback_missing_code() {
        let result = parse_callback_query("/callback?state=mystate", "mystate");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("code"));
    }

    #[test]
    fn test_token_validity_check() {
        let valid = CachedAuthCodeToken {
            access_token: "token".into(),
            refresh_token: None,
            expires_at: Instant::now() + Duration::from_secs(300),
        };
        assert!(is_token_valid(&valid, Duration::from_secs(30)));

        let expiring = CachedAuthCodeToken {
            access_token: "token".into(),
            refresh_token: None,
            expires_at: Instant::now() + Duration::from_secs(10),
        };
        assert!(!is_token_valid(&expiring, Duration::from_secs(30)));
    }
}
