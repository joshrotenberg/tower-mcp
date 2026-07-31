//! Cohesive OAuth authorization-code state machine for MCP clients.
//!
//! [`OAuthAuthorizationFlow`] composes the lower-level discovery,
//! registration, PKCE, redirect, token, refresh, persistence, and scope
//! escalation pieces into one reusable client.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use tokio::sync::{Mutex, RwLock, oneshot};

use super::oauth::{
    OAuthBearerChallenge, OAuthClientError, OAuthScopeEscalationHandler,
    OAuthScopeEscalationRequest, OAuthTokenEndpointAuthMethod, TokenProvider,
};
use super::oauth_authcode::{
    OAuthAuthorizationServerMetadata, OAuthClientRegistration, OAuthClientRegistrationMethod,
    OAuthClientRegistrationOptions, OAuthClientRegistrationStore, OAuthProtectedResourceMetadata,
};

/// HTTP method used by an OAuth protocol request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OAuthHttpMethod {
    /// HTTP GET.
    Get,
    /// HTTP POST.
    Post,
}

/// Body of an OAuth protocol HTTP request.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum OAuthHttpBody {
    /// No request body.
    Empty,
    /// `application/x-www-form-urlencoded` fields.
    Form(Vec<(String, String)>),
    /// JSON request body.
    Json(serde_json::Value),
}

/// Transport-neutral OAuth protocol HTTP request.
#[derive(Debug, Clone)]
pub struct OAuthHttpRequest {
    /// HTTP method.
    pub method: OAuthHttpMethod,
    /// Absolute request URL.
    pub url: String,
    /// Request headers.
    pub headers: Vec<(String, String)>,
    /// Request body.
    pub body: OAuthHttpBody,
}

impl OAuthHttpRequest {
    fn get(url: impl Into<String>) -> Self {
        Self {
            method: OAuthHttpMethod::Get,
            url: url.into(),
            headers: Vec::new(),
            body: OAuthHttpBody::Empty,
        }
    }

    fn post_form(url: impl Into<String>, fields: Vec<(String, String)>) -> Self {
        Self {
            method: OAuthHttpMethod::Post,
            url: url.into(),
            headers: Vec::new(),
            body: OAuthHttpBody::Form(fields),
        }
    }

    fn post_json(url: impl Into<String>, value: serde_json::Value) -> Self {
        Self {
            method: OAuthHttpMethod::Post,
            url: url.into(),
            headers: Vec::new(),
            body: OAuthHttpBody::Json(value),
        }
    }

    fn basic_auth(mut self, client_id: &str, client_secret: &str) -> Self {
        // RFC 6749 section 2.3.1 applies application/x-www-form-urlencoded
        // encoding to both credentials before constructing HTTP Basic auth.
        let client_id = urlencoding::encode(client_id);
        let client_secret = urlencoding::encode(client_secret);
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("{client_id}:{client_secret}"));
        self.headers
            .push(("authorization".to_string(), format!("Basic {encoded}")));
        self
    }
}

/// Transport-neutral OAuth protocol HTTP response.
#[derive(Debug, Clone)]
pub struct OAuthHttpResponse {
    /// Numeric HTTP status code.
    pub status: u16,
    /// Response headers. Repeated fields are retained as separate entries.
    pub headers: Vec<(String, String)>,
    /// Complete response body.
    pub body: Vec<u8>,
}

impl OAuthHttpResponse {
    /// Return whether the response status is in the 2xx range.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Iterate over values for a case-insensitive header name.
    pub fn header_values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers
            .iter()
            .filter(move |(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, OAuthClientError> {
        serde_json::from_slice(&self.body)
            .map_err(|error| OAuthClientError::InvalidResponse(error.to_string()))
    }

    fn error_body(&self) -> String {
        String::from_utf8_lossy(&self.body)
            .chars()
            .take(1024)
            .collect()
    }
}

/// Application-supplied HTTP abstraction for OAuth protocol traffic.
///
/// Implementations control proxying, TLS, telemetry, retries, and test
/// behavior without requiring the authorization state machine to depend on a
/// particular HTTP stack.
#[async_trait]
pub trait OAuthHttpClient: Send + Sync + 'static {
    /// Execute one OAuth protocol request.
    async fn execute(
        &self,
        request: OAuthHttpRequest,
    ) -> Result<OAuthHttpResponse, OAuthClientError>;
}

/// [`OAuthHttpClient`] backed by reqwest.
#[derive(Clone)]
pub struct ReqwestOAuthHttpClient {
    client: reqwest::Client,
}

impl fmt::Debug for ReqwestOAuthHttpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReqwestOAuthHttpClient")
            .finish_non_exhaustive()
    }
}

impl ReqwestOAuthHttpClient {
    /// Wrap an application-configured reqwest client.
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// Create a client that does not automatically follow redirects.
    pub fn without_redirects() -> Result<Self, OAuthClientError> {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map(Self::new)
            .map_err(|error| OAuthClientError::Http(error.to_string()))
    }
}

#[async_trait]
impl OAuthHttpClient for ReqwestOAuthHttpClient {
    async fn execute(
        &self,
        request: OAuthHttpRequest,
    ) -> Result<OAuthHttpResponse, OAuthClientError> {
        let method = match request.method {
            OAuthHttpMethod::Get => reqwest::Method::GET,
            OAuthHttpMethod::Post => reqwest::Method::POST,
        };
        let mut builder = self.client.request(method, &request.url);
        for (name, value) in request.headers {
            builder = builder.header(name, value);
        }
        builder = match request.body {
            OAuthHttpBody::Empty => builder,
            OAuthHttpBody::Form(fields) => builder.form(&fields),
            OAuthHttpBody::Json(value) => builder.json(&value),
        };
        let response = builder
            .send()
            .await
            .map_err(|error| OAuthClientError::Http(error.to_string()))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_string(), value.to_string()))
            })
            .collect();
        let body = response
            .bytes()
            .await
            .map_err(|error| OAuthClientError::Http(error.to_string()))?
            .to_vec();
        Ok(OAuthHttpResponse {
            status,
            headers,
            body,
        })
    }
}

/// Binding key that prevents token reuse across resources, issuers, or clients.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct OAuthTokenBinding {
    /// Canonical MCP protected-resource identifier.
    pub resource: String,
    /// Exact authorization-server issuer.
    pub issuer: String,
    /// OAuth client identifier.
    pub client_id: String,
}

/// Persistable OAuth authorization-code token set.
///
/// This value contains bearer credentials and must be protected as a secret.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OAuthStoredToken {
    /// Access token.
    pub access_token: String,
    /// Refresh token, when issued.
    pub refresh_token: Option<String>,
    /// Expiration time as Unix seconds.
    ///
    /// This is [`u64::MAX`] when the authorization server did not advertise
    /// an `expires_in` lifetime.
    pub expires_at: u64,
    /// Scopes represented by this token.
    pub scopes: Vec<String>,
}

impl fmt::Debug for OAuthStoredToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthStoredToken")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .finish()
    }
}

/// Persistent storage for resource/issuer/client-bound OAuth tokens.
#[async_trait]
pub trait OAuthTokenStore: Send + Sync {
    /// Load a token set for an exact binding.
    async fn load(
        &self,
        binding: &OAuthTokenBinding,
    ) -> Result<Option<OAuthStoredToken>, OAuthClientError>;

    /// Save a token set for an exact binding.
    async fn save(
        &self,
        binding: &OAuthTokenBinding,
        token: &OAuthStoredToken,
    ) -> Result<(), OAuthClientError>;

    /// Remove a token set for an exact binding.
    async fn remove(&self, binding: &OAuthTokenBinding) -> Result<(), OAuthClientError>;
}

/// Process-local OAuth token store.
#[derive(Clone, Default)]
pub struct MemoryOAuthTokenStore {
    tokens: Arc<RwLock<HashMap<OAuthTokenBinding, OAuthStoredToken>>>,
}

impl fmt::Debug for MemoryOAuthTokenStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemoryOAuthTokenStore")
            .finish_non_exhaustive()
    }
}

impl MemoryOAuthTokenStore {
    /// Create an empty process-local token store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl OAuthTokenStore for MemoryOAuthTokenStore {
    async fn load(
        &self,
        binding: &OAuthTokenBinding,
    ) -> Result<Option<OAuthStoredToken>, OAuthClientError> {
        Ok(self.tokens.read().await.get(binding).cloned())
    }

    async fn save(
        &self,
        binding: &OAuthTokenBinding,
        token: &OAuthStoredToken,
    ) -> Result<(), OAuthClientError> {
        self.tokens
            .write()
            .await
            .insert(binding.clone(), token.clone());
        Ok(())
    }

    async fn remove(&self, binding: &OAuthTokenBinding) -> Result<(), OAuthClientError> {
        self.tokens.write().await.remove(binding);
        Ok(())
    }
}

/// Persistable PKCE and CSRF state for an in-progress authorization.
///
/// This contains a PKCE verifier and possibly a client secret. Store it with
/// the same protections as OAuth credentials.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OAuthPendingAuthorizationState {
    /// CSRF state key.
    pub state: String,
    /// PKCE verifier.
    pub code_verifier: String,
    /// Exact redirect URI.
    pub redirect_uri: String,
    /// Canonical resource identifier.
    pub resource: String,
    /// Exact authorization-server issuer.
    pub issuer: String,
    /// Token endpoint selected from validated metadata.
    pub token_endpoint: String,
    /// Resolved client registration.
    pub registration: OAuthClientRegistration,
    /// Token endpoint authentication method.
    pub token_endpoint_auth_method: OAuthTokenEndpointAuthMethod,
    /// Requested scopes.
    pub scopes: Vec<String>,
    /// Whether an authorization-response `iss` parameter is required.
    pub iss_required: bool,
}

impl fmt::Debug for OAuthPendingAuthorizationState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthPendingAuthorizationState")
            .field("state", &"[REDACTED]")
            .field("code_verifier", &"[REDACTED]")
            .field("redirect_uri", &self.redirect_uri)
            .field("resource", &self.resource)
            .field("issuer", &self.issuer)
            .field("token_endpoint", &self.token_endpoint)
            .field("registration", &self.registration)
            .field(
                "token_endpoint_auth_method",
                &self.token_endpoint_auth_method,
            )
            .field("scopes", &self.scopes)
            .field("iss_required", &self.iss_required)
            .finish()
    }
}

/// Persistent storage for PKCE and CSRF state.
#[async_trait]
pub trait OAuthAuthorizationStateStore: Send + Sync {
    /// Load pending authorization state by the exact CSRF state key.
    async fn load(
        &self,
        state: &str,
    ) -> Result<Option<OAuthPendingAuthorizationState>, OAuthClientError>;

    /// Save pending authorization state.
    async fn save(&self, state: &OAuthPendingAuthorizationState) -> Result<(), OAuthClientError>;

    /// Remove consumed or abandoned state.
    async fn remove(&self, state: &str) -> Result<(), OAuthClientError>;
}

/// Process-local PKCE and CSRF state store.
#[derive(Clone, Default)]
pub struct MemoryOAuthAuthorizationStateStore {
    states: Arc<RwLock<HashMap<String, OAuthPendingAuthorizationState>>>,
}

impl fmt::Debug for MemoryOAuthAuthorizationStateStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemoryOAuthAuthorizationStateStore")
            .finish_non_exhaustive()
    }
}

impl MemoryOAuthAuthorizationStateStore {
    /// Create an empty process-local state store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl OAuthAuthorizationStateStore for MemoryOAuthAuthorizationStateStore {
    async fn load(
        &self,
        state: &str,
    ) -> Result<Option<OAuthPendingAuthorizationState>, OAuthClientError> {
        Ok(self.states.read().await.get(state).cloned())
    }

    async fn save(&self, state: &OAuthPendingAuthorizationState) -> Result<(), OAuthClientError> {
        self.states
            .write()
            .await
            .insert(state.state.clone(), state.clone());
        Ok(())
    }

    async fn remove(&self, state: &str) -> Result<(), OAuthClientError> {
        self.states.write().await.remove(state);
        Ok(())
    }
}

/// Redirect handling policy for an authorization flow.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OAuthRedirectPolicy {
    /// Host a one-shot callback on the loopback interface.
    Loopback {
        /// Requested port, or `None` for an ephemeral port.
        port: Option<u16>,
        /// Callback path, beginning with `/`.
        callback_path: String,
    },
    /// Use an application-owned redirect URI and complete the flow by passing
    /// the returned callback URL to [`OAuthAuthorizationFlow::complete_callback_url`].
    Fixed {
        /// Exact registered redirect URI.
        redirect_uri: String,
    },
}

impl OAuthRedirectPolicy {
    /// Use an ephemeral loopback port and `/callback`.
    pub fn loopback() -> Self {
        Self::Loopback {
            port: None,
            callback_path: "/callback".to_string(),
        }
    }

    /// Use a specific loopback port and callback path.
    pub fn loopback_at(port: u16, callback_path: impl Into<String>) -> Self {
        Self::Loopback {
            port: Some(port),
            callback_path: callback_path.into(),
        }
    }

    /// Use an application-owned redirect URI.
    pub fn fixed(redirect_uri: impl Into<String>) -> Self {
        Self::Fixed {
            redirect_uri: redirect_uri.into(),
        }
    }
}

/// Authorization request presented to an application or browser integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthAuthorizationRequest {
    /// URL that the user agent should open.
    pub authorization_url: String,
    /// Exact redirect URI registered for this attempt.
    pub redirect_uri: String,
    /// Canonical resource identifier.
    pub resource: String,
    /// Exact authorization-server issuer.
    pub issuer: String,
    /// Requested scopes.
    pub scopes: Vec<String>,
}

/// Result returned by an application authorization handler.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OAuthAuthorizationAction {
    /// The handler presented the URL; wait for the configured loopback callback.
    AwaitLoopback,
    /// The handler captured and returned the complete callback URL.
    CallbackUrl(String),
}

/// Application hook that presents or automates an authorization redirect.
#[async_trait]
pub trait OAuthAuthorizationHandler: Send + Sync + 'static {
    /// Present the authorization request and choose how it completes.
    async fn authorize(
        &self,
        request: OAuthAuthorizationRequest,
    ) -> Result<OAuthAuthorizationAction, OAuthClientError>;
}

/// Input to a private-key JWT signer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthClientAssertionRequest {
    /// OAuth client identifier (`iss` and `sub` in the assertion).
    pub client_id: String,
    /// Token endpoint URL (`aud` in the assertion).
    pub token_endpoint: String,
    /// Exact authorization-server issuer selected for the flow.
    pub authorization_server_issuer: String,
}

/// Application hook for `private_key_jwt` client authentication.
///
/// Implementations normally create a short-lived signed JWT containing `iss`,
/// `sub`, `aud`, `iat`, `exp`, and a unique `jti`.
#[async_trait]
pub trait OAuthClientAssertionSigner: Send + Sync + 'static {
    /// Create a client assertion for one token request.
    async fn sign_client_assertion(
        &self,
        request: OAuthClientAssertionRequest,
    ) -> Result<String, OAuthClientError>;
}

#[derive(Clone)]
struct ActiveToken {
    binding: OAuthTokenBinding,
    token: OAuthStoredToken,
    token_endpoint: String,
    registration: OAuthClientRegistration,
    auth_method: OAuthTokenEndpointAuthMethod,
}

struct FlowInner {
    resource_url: String,
    registration_options: OAuthClientRegistrationOptions,
    pre_registered_client: Option<(String, Option<String>)>,
    registration_store: Arc<dyn OAuthClientRegistrationStore>,
    token_store: Arc<dyn OAuthTokenStore>,
    state_store: Arc<dyn OAuthAuthorizationStateStore>,
    http: Arc<dyn OAuthHttpClient>,
    redirect_policy: OAuthRedirectPolicy,
    preferred_issuer: Option<String>,
    refresh_buffer: Duration,
    authorization_handler: Option<Arc<dyn OAuthAuthorizationHandler>>,
    assertion_signer: Option<Arc<dyn OAuthClientAssertionSigner>>,
    current: RwLock<Option<ActiveToken>>,
    authorization_lock: Mutex<()>,
    refresh_lock: Mutex<()>,
}

/// Reusable OAuth authorization-code state machine and token provider.
///
/// Build this type with [`OAuthAuthorizationFlow::builder`], call
/// [`authorize`](Self::authorize) for a fully driven flow or
/// [`begin`](Self::begin) for explicit pending/authorized states, then install
/// the same value as both [`TokenProvider`] and [`OAuthScopeEscalationHandler`].
#[derive(Clone)]
pub struct OAuthAuthorizationFlow {
    inner: Arc<FlowInner>,
}

impl fmt::Debug for OAuthAuthorizationFlow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthAuthorizationFlow")
            .field("resource_url", &self.inner.resource_url)
            .field("redirect_policy", &self.inner.redirect_policy)
            .field("preferred_issuer", &self.inner.preferred_issuer)
            .finish_non_exhaustive()
    }
}

/// Builder for [`OAuthAuthorizationFlow`].
pub struct OAuthAuthorizationFlowBuilder {
    resource_url: String,
    registration_options: OAuthClientRegistrationOptions,
    pre_registered_client: Option<(String, Option<String>)>,
    registration_store: Option<Arc<dyn OAuthClientRegistrationStore>>,
    token_store: Option<Arc<dyn OAuthTokenStore>>,
    state_store: Option<Arc<dyn OAuthAuthorizationStateStore>>,
    http: Option<Arc<dyn OAuthHttpClient>>,
    redirect_policy: Option<OAuthRedirectPolicy>,
    preferred_issuer: Option<String>,
    refresh_buffer: Duration,
    authorization_handler: Option<Arc<dyn OAuthAuthorizationHandler>>,
    assertion_signer: Option<Arc<dyn OAuthClientAssertionSigner>>,
}

impl fmt::Debug for OAuthAuthorizationFlowBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthAuthorizationFlowBuilder")
            .field("resource_url", &self.resource_url)
            .field("registration_options", &self.registration_options)
            .field("redirect_policy", &self.redirect_policy)
            .field("preferred_issuer", &self.preferred_issuer)
            .field("refresh_buffer", &self.refresh_buffer)
            .finish_non_exhaustive()
    }
}

impl OAuthAuthorizationFlowBuilder {
    /// Configure available client-registration mechanisms.
    pub fn registration_options(mut self, options: OAuthClientRegistrationOptions) -> Self {
        self.registration_options = options;
        self
    }

    /// Configure pre-registered credentials and bind them to the issuer selected
    /// during discovery.
    ///
    /// This is the convenient form for applications that know their client
    /// credentials before they know which authorization-server issuer the
    /// protected resource will advertise. It takes priority over CIMD and DCR,
    /// just like [`OAuthClientRegistrationOptions::with_pre_registered`].
    pub fn pre_registered_client(
        mut self,
        client_id: impl Into<String>,
        client_secret: Option<String>,
    ) -> Self {
        self.pre_registered_client = Some((client_id.into(), client_secret));
        self
    }

    /// Configure issuer-bound client-credential persistence.
    pub fn registration_store(
        mut self,
        store: impl OAuthClientRegistrationStore + 'static,
    ) -> Self {
        self.registration_store = Some(Arc::new(store));
        self
    }

    /// Configure token persistence.
    pub fn token_store(mut self, store: impl OAuthTokenStore + 'static) -> Self {
        self.token_store = Some(Arc::new(store));
        self
    }

    /// Configure persisted PKCE and CSRF state.
    pub fn state_store(mut self, store: impl OAuthAuthorizationStateStore + 'static) -> Self {
        self.state_store = Some(Arc::new(store));
        self
    }

    /// Configure the OAuth protocol HTTP implementation.
    pub fn http_client(mut self, client: impl OAuthHttpClient) -> Self {
        self.http = Some(Arc::new(client));
        self
    }

    /// Configure explicit redirect handling.
    pub fn redirect_policy(mut self, policy: OAuthRedirectPolicy) -> Self {
        self.redirect_policy = Some(policy);
        self
    }

    /// Select one exact issuer when the resource advertises multiple servers.
    pub fn preferred_authorization_server(mut self, issuer: impl Into<String>) -> Self {
        self.preferred_issuer = Some(issuer.into());
        self
    }

    /// Set the pre-expiry refresh buffer.
    pub fn refresh_buffer(mut self, buffer: Duration) -> Self {
        self.refresh_buffer = buffer;
        self
    }

    /// Configure automatic browser/headless authorization handling.
    pub fn authorization_handler(mut self, handler: impl OAuthAuthorizationHandler) -> Self {
        self.authorization_handler = Some(Arc::new(handler));
        self
    }

    /// Configure `private_key_jwt` assertion signing.
    pub fn client_assertion_signer(mut self, signer: impl OAuthClientAssertionSigner) -> Self {
        self.assertion_signer = Some(Arc::new(signer));
        self
    }

    /// Build the state machine.
    ///
    /// # Errors
    ///
    /// Returns an error when no redirect policy was supplied or when the
    /// default reqwest client cannot be constructed.
    pub fn build(self) -> Result<OAuthAuthorizationFlow, OAuthClientError> {
        let redirect_policy = self.redirect_policy.ok_or_else(|| {
            OAuthClientError::BuildError(
                "OAuthAuthorizationFlow requires an explicit redirect policy".to_string(),
            )
        })?;
        validate_redirect_policy(&redirect_policy)?;
        let http = match self.http {
            Some(http) => http,
            None => Arc::new(ReqwestOAuthHttpClient::without_redirects()?),
        };
        Ok(OAuthAuthorizationFlow {
            inner: Arc::new(FlowInner {
                resource_url: self.resource_url,
                registration_options: self.registration_options,
                pre_registered_client: self.pre_registered_client,
                registration_store: self.registration_store.unwrap_or_else(|| {
                    Arc::new(super::oauth_authcode::MemoryOAuthClientRegistrationStore::new())
                }),
                token_store: self
                    .token_store
                    .unwrap_or_else(|| Arc::new(MemoryOAuthTokenStore::new())),
                state_store: self
                    .state_store
                    .unwrap_or_else(|| Arc::new(MemoryOAuthAuthorizationStateStore::new())),
                http,
                redirect_policy,
                preferred_issuer: self.preferred_issuer,
                refresh_buffer: self.refresh_buffer,
                authorization_handler: self.authorization_handler,
                assertion_signer: self.assertion_signer,
                current: RwLock::new(None),
                authorization_lock: Mutex::new(()),
                refresh_lock: Mutex::new(()),
            }),
        })
    }
}

fn validate_redirect_policy(policy: &OAuthRedirectPolicy) -> Result<(), OAuthClientError> {
    let (redirect_uri, callback_path) = match policy {
        OAuthRedirectPolicy::Fixed { redirect_uri } => (Some(redirect_uri.as_str()), None),
        OAuthRedirectPolicy::Loopback { callback_path, .. } => (None, Some(callback_path)),
    };
    if let Some(uri) = redirect_uri {
        let parsed = reqwest::Url::parse(uri)
            .map_err(|error| OAuthClientError::BuildError(error.to_string()))?;
        if parsed.fragment().is_some() {
            return Err(OAuthClientError::BuildError(
                "OAuth redirect URI must not contain a fragment".to_string(),
            ));
        }
    }
    if let Some(path) = callback_path
        && (!path.starts_with('/') || path.contains('?') || path.contains('#'))
    {
        return Err(OAuthClientError::BuildError(
            "loopback callback path must begin with `/` and contain no query or fragment"
                .to_string(),
        ));
    }
    Ok(())
}

/// Result of beginning an OAuth authorization attempt.
#[non_exhaustive]
pub enum OAuthAuthorizationStart {
    /// A persisted, sufficiently scoped token was restored without redirecting.
    Authorized {
        /// Scopes restored from the token store.
        scopes: Vec<String>,
    },
    /// User-agent authorization is required.
    Pending(OAuthPendingAuthorization),
}

impl fmt::Debug for OAuthAuthorizationStart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authorized { scopes } => formatter
                .debug_struct("Authorized")
                .field("scopes", scopes)
                .finish(),
            Self::Pending(pending) => formatter.debug_tuple("Pending").field(pending).finish(),
        }
    }
}

/// An authorization attempt waiting for a callback.
pub struct OAuthPendingAuthorization {
    flow: OAuthAuthorizationFlow,
    request: OAuthAuthorizationRequest,
    state: String,
    callback_rx: Mutex<Option<oneshot::Receiver<String>>>,
}

impl fmt::Debug for OAuthPendingAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthPendingAuthorization")
            .field("request", &self.request)
            .field("state", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl OAuthPendingAuthorization {
    /// Authorization request to present to a user agent or headless harness.
    pub fn request(&self) -> &OAuthAuthorizationRequest {
        &self.request
    }

    /// Complete this attempt with the callback URL returned by the AS.
    pub async fn complete_callback_url(self, callback_url: &str) -> Result<(), OAuthClientError> {
        self.flow
            .complete_callback_url_for_state(callback_url, Some(&self.state))
            .await
    }

    /// Wait for a configured loopback redirect and complete the attempt.
    pub async fn wait_for_callback(self) -> Result<(), OAuthClientError> {
        self.wait_for_callback_with_timeout(Duration::from_secs(300))
            .await
    }

    /// Wait for a loopback redirect with a custom timeout.
    pub async fn wait_for_callback_with_timeout(
        self,
        timeout: Duration,
    ) -> Result<(), OAuthClientError> {
        let receiver = self.callback_rx.lock().await.take().ok_or_else(|| {
            OAuthClientError::Redirect(
                "this redirect policy does not own a loopback callback".to_string(),
            )
        })?;
        let callback_url = tokio::time::timeout(timeout, receiver)
            .await
            .map_err(|_| OAuthClientError::Redirect("OAuth callback timed out".to_string()))?
            .map_err(|_| {
                OAuthClientError::Redirect("OAuth callback listener closed".to_string())
            })?;
        self.flow
            .complete_callback_url_for_state(&callback_url, Some(&self.state))
            .await
    }
}

impl OAuthAuthorizationFlow {
    /// Begin building a state machine for `resource_url`.
    pub fn builder(resource_url: impl Into<String>) -> OAuthAuthorizationFlowBuilder {
        OAuthAuthorizationFlowBuilder {
            resource_url: resource_url.into(),
            registration_options: OAuthClientRegistrationOptions::new(),
            pre_registered_client: None,
            registration_store: None,
            token_store: None,
            state_store: None,
            http: None,
            redirect_policy: None,
            preferred_issuer: None,
            refresh_buffer: Duration::from_secs(30),
            authorization_handler: None,
            assertion_signer: None,
        }
    }

    /// Discover, register, restore or create authorization state, and return
    /// the next explicit state.
    pub async fn begin<I, S>(&self, scopes: I) -> Result<OAuthAuthorizationStart, OAuthClientError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.begin_with_challenge(unique_scopes(scopes), None).await
    }

    /// Drive a complete authorization using the configured
    /// [`OAuthAuthorizationHandler`].
    pub async fn authorize<I, S>(&self, scopes: I) -> Result<(), OAuthClientError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.authorize_with_challenge(unique_scopes(scopes), None)
            .await
    }

    async fn authorize_with_challenge(
        &self,
        scopes: Vec<String>,
        challenge: Option<OAuthBearerChallenge>,
    ) -> Result<(), OAuthClientError> {
        let _guard = self.inner.authorization_lock.lock().await;
        let start = self.begin_with_challenge(scopes, challenge).await?;
        let OAuthAuthorizationStart::Pending(pending) = start else {
            return Ok(());
        };
        let handler = self.inner.authorization_handler.as_ref().ok_or_else(|| {
            OAuthClientError::Redirect(
                "authorization is pending; configure an OAuthAuthorizationHandler or use begin()"
                    .to_string(),
            )
        })?;
        match handler.authorize(pending.request().clone()).await? {
            OAuthAuthorizationAction::AwaitLoopback => pending.wait_for_callback().await,
            OAuthAuthorizationAction::CallbackUrl(url) => pending.complete_callback_url(&url).await,
        }
    }

    async fn begin_with_challenge(
        &self,
        explicit_scopes: Vec<String>,
        challenge: Option<OAuthBearerChallenge>,
    ) -> Result<OAuthAuthorizationStart, OAuthClientError> {
        let discovery = discover_with_http(
            &self.inner.resource_url,
            challenge,
            self.inner.http.as_ref(),
        )
        .await?;
        let metadata =
            select_authorization_server(&discovery, self.inner.preferred_issuer.as_deref())?;
        require_s256(&metadata)?;

        let state = random_urlsafe(16);
        let code_verifier = random_urlsafe(32);
        let code_challenge = pkce_challenge(&code_verifier);
        let (redirect_uri, callback_rx) =
            prepare_redirect(&self.inner.redirect_policy, &state).await?;

        let mut registration_options = self.inner.registration_options.clone();
        if registration_options.pre_registered.is_none()
            && let Some((client_id, client_secret)) = &self.inner.pre_registered_client
        {
            registration_options.pre_registered = Some(OAuthClientRegistration::pre_registered(
                metadata.issuer.clone(),
                client_id.clone(),
                client_secret.clone(),
            ));
        }
        if let Some(dynamic) = registration_options.dynamic_registration.as_mut() {
            dynamic.redirect_uris = vec![redirect_uri.clone()];
            if metadata
                .grant_types_supported
                .iter()
                .any(|grant| grant == "refresh_token")
                && !dynamic
                    .grant_types
                    .iter()
                    .any(|grant| grant == "refresh_token")
            {
                dynamic.grant_types.push("refresh_token".to_string());
            }
            dynamic.token_endpoint_auth_method = preferred_registration_auth_method(
                &metadata.token_endpoint_auth_methods_supported,
                self.inner.assertion_signer.is_some(),
            )
            .to_string();
        }
        let registration = resolve_registration_with_http(
            self.inner.http.as_ref(),
            &metadata,
            &registration_options,
            self.inner.registration_store.as_ref(),
        )
        .await?;

        let auth_method = OAuthTokenEndpointAuthMethod::select_with_private_key(
            &metadata.token_endpoint_auth_methods_supported,
            registration.client_secret().is_some(),
            self.inner.assertion_signer.is_some(),
        )?;
        let scopes = select_scopes(&explicit_scopes, &discovery, &metadata);
        let binding = OAuthTokenBinding {
            resource: discovery.resource.clone(),
            issuer: metadata.issuer.clone(),
            client_id: registration.client_id().to_string(),
        };

        if let Some(token) = self.inner.token_store.load(&binding).await?
            && token_is_valid(&token, self.inner.refresh_buffer)
            && scopes_are_covered(&scopes, &token.scopes)
        {
            *self.inner.current.write().await = Some(ActiveToken {
                binding,
                token: token.clone(),
                token_endpoint: metadata.token_endpoint,
                registration,
                auth_method,
            });
            return Ok(OAuthAuthorizationStart::Authorized {
                scopes: token.scopes,
            });
        }

        let pending_state = OAuthPendingAuthorizationState {
            state: state.clone(),
            code_verifier,
            redirect_uri: redirect_uri.clone(),
            resource: discovery.resource.clone(),
            issuer: metadata.issuer.clone(),
            token_endpoint: metadata.token_endpoint.clone(),
            registration: registration.clone(),
            token_endpoint_auth_method: auth_method,
            scopes: scopes.clone(),
            iss_required: metadata.authorization_response_iss_parameter_supported,
        };
        self.inner.state_store.save(&pending_state).await?;

        let mut authorization_url = reqwest::Url::parse(&metadata.authorization_endpoint)
            .map_err(|error| OAuthClientError::Discovery(error.to_string()))?;
        {
            let mut query = authorization_url.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", registration.client_id())
                .append_pair("redirect_uri", &redirect_uri)
                .append_pair("state", &state)
                .append_pair("code_challenge", &code_challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("resource", &discovery.resource);
            if !scopes.is_empty() {
                query.append_pair("scope", &scopes.join(" "));
            }
        }
        Ok(OAuthAuthorizationStart::Pending(
            OAuthPendingAuthorization {
                flow: self.clone(),
                request: OAuthAuthorizationRequest {
                    authorization_url: authorization_url.to_string(),
                    redirect_uri,
                    resource: discovery.resource,
                    issuer: metadata.issuer,
                    scopes,
                },
                state,
                callback_rx: Mutex::new(callback_rx),
            },
        ))
    }

    /// Resume a persisted authorization attempt from a callback URL.
    pub async fn complete_callback_url(&self, callback_url: &str) -> Result<(), OAuthClientError> {
        self.complete_callback_url_for_state(callback_url, None)
            .await
    }

    async fn complete_callback_url_for_state(
        &self,
        callback_url: &str,
        expected_state: Option<&str>,
    ) -> Result<(), OAuthClientError> {
        let parsed = parse_callback_url(callback_url)?;
        if let Some(expected) = expected_state
            && parsed.state != expected
        {
            return Err(OAuthClientError::InvalidResponse(
                "OAuth callback state mismatch".to_string(),
            ));
        }
        let pending = self
            .inner
            .state_store
            .load(&parsed.state)
            .await?
            .ok_or_else(|| {
                OAuthClientError::StateStore(
                    "no persisted OAuth authorization matches the callback state".to_string(),
                )
            })?;
        validate_callback_target(callback_url, &pending.redirect_uri)?;
        validate_callback_issuer(
            parsed.issuer.as_deref(),
            &pending.issuer,
            pending.iss_required,
        )?;

        let fields = vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".to_string(), parsed.code),
            ("redirect_uri".to_string(), pending.redirect_uri.clone()),
            ("code_verifier".to_string(), pending.code_verifier.clone()),
            ("resource".to_string(), pending.resource.clone()),
        ];
        let response = send_token_request(
            self.inner.http.as_ref(),
            &pending.token_endpoint,
            &pending.issuer,
            &pending.registration,
            pending.token_endpoint_auth_method,
            fields,
            self.inner.assertion_signer.as_deref(),
        )
        .await?;
        let token = token_from_response(response, &pending.scopes, None)?;
        let binding = OAuthTokenBinding {
            resource: pending.resource.clone(),
            issuer: pending.issuer.clone(),
            client_id: pending.registration.client_id().to_string(),
        };
        self.inner.token_store.save(&binding, &token).await?;
        self.inner.state_store.remove(&parsed.state).await?;
        *self.inner.current.write().await = Some(ActiveToken {
            binding,
            token,
            token_endpoint: pending.token_endpoint,
            registration: pending.registration,
            auth_method: pending.token_endpoint_auth_method,
        });
        Ok(())
    }

    /// Return the currently authorized scope set, if any.
    pub async fn authorized_scopes(&self) -> Option<Vec<String>> {
        self.inner
            .current
            .read()
            .await
            .as_ref()
            .map(|active| active.token.scopes.clone())
    }
}

#[async_trait]
impl TokenProvider for OAuthAuthorizationFlow {
    async fn get_token(&self) -> Result<String, OAuthClientError> {
        if let Some(active) = self.inner.current.read().await.as_ref()
            && token_is_valid(&active.token, self.inner.refresh_buffer)
        {
            return Ok(active.token.access_token.clone());
        }

        let _guard = self.inner.refresh_lock.lock().await;
        let active = self.inner.current.read().await.clone().ok_or_else(|| {
            OAuthClientError::TokenRequest(
                "OAuthAuthorizationFlow is not authorized; call authorize() or begin()".to_string(),
            )
        })?;
        if token_is_valid(&active.token, self.inner.refresh_buffer) {
            return Ok(active.token.access_token);
        }
        let refresh_token = active.token.refresh_token.as_deref().ok_or_else(|| {
            OAuthClientError::TokenRequest(
                "OAuth access token expired and no refresh token is available".to_string(),
            )
        })?;
        let mut fields = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
            ("resource".to_string(), active.binding.resource.clone()),
        ];
        if !active.token.scopes.is_empty() {
            fields.push(("scope".to_string(), active.token.scopes.join(" ")));
        }
        let response = send_token_request(
            self.inner.http.as_ref(),
            &active.token_endpoint,
            &active.binding.issuer,
            &active.registration,
            active.auth_method,
            fields,
            self.inner.assertion_signer.as_deref(),
        )
        .await?;
        let token = token_from_response(
            response,
            &active.token.scopes,
            active.token.refresh_token.clone(),
        )?;
        self.inner.token_store.save(&active.binding, &token).await?;
        let access_token = token.access_token.clone();
        *self.inner.current.write().await = Some(ActiveToken { token, ..active });
        Ok(access_token)
    }
}

#[async_trait]
impl OAuthScopeEscalationHandler for OAuthAuthorizationFlow {
    async fn reauthorize(
        &self,
        request: OAuthScopeEscalationRequest,
    ) -> Result<(), OAuthClientError> {
        if request.resource != self.inner.resource_url {
            return Err(OAuthClientError::ScopeEscalation(format!(
                "scope challenge resource `{}` does not match flow resource `{}`",
                request.resource, self.inner.resource_url
            )));
        }
        let challenge = OAuthBearerChallenge {
            error: Some("insufficient_scope".to_string()),
            scopes: request.challenge.required_scopes,
            resource_metadata: request.challenge.resource_metadata,
            error_description: request.challenge.error_description,
        };
        self.authorize_with_challenge(request.requested_scopes, Some(challenge))
            .await
            .map_err(|error| OAuthClientError::ScopeEscalation(error.to_string()))
    }
}

#[derive(Debug)]
struct FlowDiscovery {
    resource: String,
    protected_resource: OAuthProtectedResourceMetadata,
    authorization_servers: Vec<OAuthAuthorizationServerMetadata>,
    challenge: Option<OAuthBearerChallenge>,
}

async fn discover_with_http(
    resource_url: &str,
    challenge: Option<OAuthBearerChallenge>,
    http: &dyn OAuthHttpClient,
) -> Result<FlowDiscovery, OAuthClientError> {
    let challenge = match challenge {
        Some(challenge) => Some(challenge),
        None => {
            // Probe a protected MCP operation rather than `initialize`, which
            // servers commonly leave public. A POST also works with MCP
            // endpoints that do not implement GET and therefore expose their
            // RFC 9728 discovery hint only on normal protocol requests.
            let mut request = OAuthHttpRequest::post_json(
                resource_url,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "tower-mcp-oauth-discovery",
                    "method": "tools/list",
                    "params": {}
                }),
            );
            request.headers.push((
                "accept".to_string(),
                "application/json, text/event-stream".to_string(),
            ));
            let response = http.execute(request).await?;
            response
                .header_values("www-authenticate")
                .find_map(OAuthBearerChallenge::from_www_authenticate)
        }
    };
    let protected_resource = if let Some(url) = challenge
        .as_ref()
        .and_then(|challenge| challenge.resource_metadata.as_deref())
    {
        fetch_json(http, url).await?
    } else {
        let mut discovered: Option<OAuthProtectedResourceMetadata> = None;
        for url in protected_resource_metadata_urls(resource_url)? {
            if let Ok(metadata) = fetch_json(http, &url).await {
                discovered = Some(metadata);
                break;
            }
        }
        discovered.ok_or_else(|| {
            OAuthClientError::Discovery(format!(
                "could not discover Protected Resource Metadata for `{resource_url}`"
            ))
        })?
    };
    validate_resource_identifier(resource_url, &protected_resource.resource)?;
    if protected_resource.authorization_servers.is_empty() {
        return Err(OAuthClientError::Discovery(
            "protected resource metadata omitted authorization_servers".to_string(),
        ));
    }

    let mut authorization_servers = Vec::new();
    let mut last_error = None;
    for issuer in &protected_resource.authorization_servers {
        match discover_authorization_server(http, issuer).await {
            Ok(metadata) => authorization_servers.push(metadata),
            Err(error) => last_error = Some(error),
        }
    }
    if authorization_servers.is_empty() {
        return Err(last_error.unwrap_or_else(|| {
            OAuthClientError::Discovery(
                "protected resource advertised no usable authorization server".to_string(),
            )
        }));
    }
    Ok(FlowDiscovery {
        resource: protected_resource.resource.clone(),
        protected_resource,
        authorization_servers,
        challenge,
    })
}

async fn discover_authorization_server(
    http: &dyn OAuthHttpClient,
    issuer: &str,
) -> Result<OAuthAuthorizationServerMetadata, OAuthClientError> {
    let mut last_error = None;
    for url in authorization_server_metadata_urls(issuer)? {
        match fetch_json::<OAuthAuthorizationServerMetadata>(http, &url).await {
            Ok(metadata) if metadata.issuer == issuer => return Ok(metadata),
            Ok(metadata) => {
                last_error = Some(OAuthClientError::Discovery(format!(
                    "authorization server issuer mismatch: expected `{issuer}`, got `{}`",
                    metadata.issuer
                )))
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        OAuthClientError::Discovery(format!(
            "could not discover authorization server metadata for `{issuer}`"
        ))
    }))
}

async fn fetch_json<T: serde::de::DeserializeOwned>(
    http: &dyn OAuthHttpClient,
    url: &str,
) -> Result<T, OAuthClientError> {
    let response = http.execute(OAuthHttpRequest::get(url)).await?;
    if !response.is_success() {
        return Err(OAuthClientError::Discovery(format!(
            "GET `{url}` returned HTTP {}: {}",
            response.status,
            response.error_body()
        )));
    }
    response.json()
}

fn protected_resource_metadata_urls(resource_url: &str) -> Result<Vec<String>, OAuthClientError> {
    let parsed = reqwest::Url::parse(resource_url)
        .map_err(|error| OAuthClientError::Discovery(error.to_string()))?;
    let origin = parsed.origin().ascii_serialization();
    let path = parsed.path();
    let mut urls = Vec::new();
    if !path.is_empty() && path != "/" {
        urls.push(format!(
            "{origin}/.well-known/oauth-protected-resource{path}"
        ));
    }
    urls.push(format!("{origin}/.well-known/oauth-protected-resource"));
    urls.dedup();
    Ok(urls)
}

fn authorization_server_metadata_urls(issuer: &str) -> Result<Vec<String>, OAuthClientError> {
    let parsed = reqwest::Url::parse(issuer)
        .map_err(|error| OAuthClientError::Discovery(error.to_string()))?;
    let origin = parsed.origin().ascii_serialization();
    let path = parsed.path();
    let trimmed = issuer.trim_end_matches('/');
    let mut urls = Vec::new();
    if !path.is_empty() && path != "/" {
        urls.push(format!(
            "{origin}/.well-known/oauth-authorization-server{path}"
        ));
        urls.push(format!("{origin}/.well-known/openid-configuration{path}"));
        urls.push(format!("{trimmed}/.well-known/openid-configuration"));
    } else {
        urls.push(format!("{origin}/.well-known/oauth-authorization-server"));
        urls.push(format!("{origin}/.well-known/openid-configuration"));
    }
    urls.dedup();
    Ok(urls)
}

fn validate_resource_identifier(expected: &str, actual: &str) -> Result<(), OAuthClientError> {
    let expected = reqwest::Url::parse(expected)
        .map_err(|error| OAuthClientError::Discovery(error.to_string()))?;
    let actual = reqwest::Url::parse(actual)
        .map_err(|error| OAuthClientError::Discovery(error.to_string()))?;
    // An origin-level canonical resource may cover a more specific MCP
    // endpoint on that origin (for example resource `https://example.com`
    // with endpoint `https://example.com/mcp`). A non-root resource path must
    // be an exact path-segment prefix, never a merely textual prefix.
    let expected_path = expected.path();
    let actual_path = actual.path();
    let path_matches = actual_path == "/"
        || expected_path == actual_path
        || (actual_path.ends_with('/') && expected_path.starts_with(actual_path))
        || expected_path
            .strip_prefix(actual_path)
            .is_some_and(|suffix| suffix.starts_with('/'));
    let query_matches = actual.query().is_none() || actual.query() == expected.query();
    let matches = actual.fragment().is_none()
        && expected.scheme() == actual.scheme()
        && expected.host_str() == actual.host_str()
        && expected.port_or_known_default() == actual.port_or_known_default()
        && path_matches
        && query_matches;
    if matches {
        Ok(())
    } else {
        Err(OAuthClientError::Discovery(format!(
            "protected resource mismatch: expected `{expected}`, got `{actual}`"
        )))
    }
}

fn select_authorization_server(
    discovery: &FlowDiscovery,
    preferred_issuer: Option<&str>,
) -> Result<OAuthAuthorizationServerMetadata, OAuthClientError> {
    match preferred_issuer {
        Some(issuer) => discovery
            .authorization_servers
            .iter()
            .find(|metadata| metadata.issuer == issuer)
            .cloned()
            .ok_or_else(|| {
                OAuthClientError::Discovery(format!(
                    "preferred authorization server `{issuer}` was not advertised"
                ))
            }),
        None => discovery
            .authorization_servers
            .first()
            .cloned()
            .ok_or_else(|| OAuthClientError::Discovery("no authorization server found".into())),
    }
}

fn require_s256(metadata: &OAuthAuthorizationServerMetadata) -> Result<(), OAuthClientError> {
    if metadata
        .code_challenge_methods_supported
        .iter()
        .any(|method| method == "S256")
    {
        Ok(())
    } else {
        Err(OAuthClientError::Discovery(format!(
            "authorization server `{}` does not advertise PKCE S256",
            metadata.issuer
        )))
    }
}

fn select_scopes(
    explicit: &[String],
    discovery: &FlowDiscovery,
    metadata: &OAuthAuthorizationServerMetadata,
) -> Vec<String> {
    let selected = if !explicit.is_empty() {
        explicit.to_vec()
    } else if let Some(challenge) = &discovery.challenge
        && !challenge.scopes.is_empty()
    {
        challenge.scopes.clone()
    } else if !discovery.protected_resource.scopes_supported.is_empty() {
        discovery.protected_resource.scopes_supported.clone()
    } else {
        metadata.scopes_supported.clone()
    };
    let mut selected = unique_scopes(selected);
    let refresh_supported = metadata.grant_types_supported.is_empty()
        || metadata
            .grant_types_supported
            .iter()
            .any(|grant| grant == "refresh_token");
    if refresh_supported
        && metadata
            .scopes_supported
            .iter()
            .any(|scope| scope == "offline_access")
        && !selected.iter().any(|scope| scope == "offline_access")
    {
        selected.push("offline_access".to_string());
    }
    selected
}

fn preferred_registration_auth_method(
    advertised: &[String],
    has_private_key_signer: bool,
) -> &'static str {
    if has_private_key_signer && advertised.iter().any(|method| method == "private_key_jwt") {
        "private_key_jwt"
    } else if advertised
        .iter()
        .any(|method| method == "client_secret_basic")
    {
        "client_secret_basic"
    } else if advertised
        .iter()
        .any(|method| method == "client_secret_post")
    {
        "client_secret_post"
    } else {
        "none"
    }
}

async fn resolve_registration_with_http(
    http: &dyn OAuthHttpClient,
    metadata: &OAuthAuthorizationServerMetadata,
    options: &OAuthClientRegistrationOptions,
    store: &dyn OAuthClientRegistrationStore,
) -> Result<OAuthClientRegistration, OAuthClientError> {
    if let Some(registration) = &options.pre_registered {
        if registration.method() != OAuthClientRegistrationMethod::PreRegistered {
            return Err(OAuthClientError::BuildError(
                "pre_registered must contain pre-registered credentials".to_string(),
            ));
        }
        if registration.bound_issuer() != Some(metadata.issuer.as_str()) {
            return Err(OAuthClientError::BuildError(format!(
                "pre-registered credentials are bound to issuer {:?}, not `{}`",
                registration.bound_issuer(),
                metadata.issuer
            )));
        }
        return Ok(registration.clone());
    }

    if metadata.client_id_metadata_document_supported
        && let Some(client_id) = &options.client_id_metadata_document
    {
        validate_cimd_url(client_id)?;
        return Ok(OAuthClientRegistration::client_id_metadata_document(
            client_id.clone(),
        ));
    }

    if options.dynamic_registration.is_some()
        && let Some(registration) = store.load(&metadata.issuer).await?
    {
        if registration.method() != OAuthClientRegistrationMethod::Dynamic
            || registration.bound_issuer() != Some(metadata.issuer.as_str())
        {
            return Err(OAuthClientError::CredentialStore(format!(
                "stored registration is not dynamically bound to `{}`",
                metadata.issuer
            )));
        }
        return Ok(registration);
    }

    if let (Some(endpoint), Some(request)) = (
        metadata.registration_endpoint.as_deref(),
        options.dynamic_registration.as_ref(),
    ) {
        if request.redirect_uris.is_empty() {
            return Err(OAuthClientError::BuildError(
                "dynamic registration requires a redirect URI".to_string(),
            ));
        }
        let value = serde_json::to_value(request)
            .map_err(|error| OAuthClientError::Registration(error.to_string()))?;
        let response = http
            .execute(OAuthHttpRequest::post_json(endpoint, value))
            .await?;
        if !response.is_success() {
            return Err(OAuthClientError::Registration(format!(
                "dynamic registration returned HTTP {}: {}",
                response.status,
                response.error_body()
            )));
        }
        #[derive(serde::Deserialize)]
        struct RegistrationResponse {
            client_id: String,
            client_secret: Option<String>,
        }
        let registered: RegistrationResponse = response.json()?;
        let registration = OAuthClientRegistration::dynamically_registered(
            metadata.issuer.clone(),
            registered.client_id,
            registered.client_secret,
        );
        store.save(&metadata.issuer, &registration).await?;
        return Ok(registration);
    }

    Err(OAuthClientError::BuildError(
        "authorization server supports none of the configured client registration mechanisms"
            .to_string(),
    ))
}

fn validate_cimd_url(client_id: &str) -> Result<(), OAuthClientError> {
    let url = reqwest::Url::parse(client_id).map_err(|error| {
        OAuthClientError::BuildError(format!("invalid CIMD client ID `{client_id}`: {error}"))
    })?;
    if url.scheme() != "https" || url.path() == "/" {
        return Err(OAuthClientError::BuildError(format!(
            "CIMD client ID `{client_id}` must use HTTPS and contain a path"
        )));
    }
    Ok(())
}

async fn send_token_request(
    http: &dyn OAuthHttpClient,
    token_endpoint: &str,
    issuer: &str,
    registration: &OAuthClientRegistration,
    method: OAuthTokenEndpointAuthMethod,
    mut fields: Vec<(String, String)>,
    assertion_signer: Option<&dyn OAuthClientAssertionSigner>,
) -> Result<OAuthHttpResponse, OAuthClientError> {
    let mut request = OAuthHttpRequest::post_form(token_endpoint, Vec::new());
    match method {
        OAuthTokenEndpointAuthMethod::None => {
            fields.push((
                "client_id".to_string(),
                registration.client_id().to_string(),
            ));
        }
        OAuthTokenEndpointAuthMethod::ClientSecretBasic => {
            let secret = registration.client_secret().ok_or_else(|| {
                OAuthClientError::BuildError(
                    "client_secret_basic selected without a client secret".to_string(),
                )
            })?;
            request = request.basic_auth(registration.client_id(), secret);
        }
        OAuthTokenEndpointAuthMethod::ClientSecretPost => {
            let secret = registration.client_secret().ok_or_else(|| {
                OAuthClientError::BuildError(
                    "client_secret_post selected without a client secret".to_string(),
                )
            })?;
            fields.push((
                "client_id".to_string(),
                registration.client_id().to_string(),
            ));
            fields.push(("client_secret".to_string(), secret.to_string()));
        }
        OAuthTokenEndpointAuthMethod::PrivateKeyJwt => {
            let signer = assertion_signer.ok_or_else(|| {
                OAuthClientError::BuildError(
                    "private_key_jwt selected without a client assertion signer".to_string(),
                )
            })?;
            let assertion = signer
                .sign_client_assertion(OAuthClientAssertionRequest {
                    client_id: registration.client_id().to_string(),
                    token_endpoint: token_endpoint.to_string(),
                    authorization_server_issuer: issuer.to_string(),
                })
                .await?;
            fields.push((
                "client_id".to_string(),
                registration.client_id().to_string(),
            ));
            fields.push((
                "client_assertion_type".to_string(),
                "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".to_string(),
            ));
            fields.push(("client_assertion".to_string(), assertion));
        }
    }
    request.body = OAuthHttpBody::Form(fields);
    let response = http.execute(request).await?;
    if !response.is_success() {
        return Err(OAuthClientError::TokenRequest(format!(
            "token endpoint returned HTTP {}: {}",
            response.status,
            response.error_body()
        )));
    }
    Ok(response)
}

fn token_from_response(
    response: OAuthHttpResponse,
    requested_scopes: &[String],
    previous_refresh_token: Option<String>,
) -> Result<OAuthStoredToken, OAuthClientError> {
    #[derive(serde::Deserialize)]
    struct TokenResponse {
        access_token: String,
        token_type: String,
        expires_in: Option<u64>,
        refresh_token: Option<String>,
        scope: Option<String>,
    }
    let response: TokenResponse = response.json()?;
    if !response.token_type.eq_ignore_ascii_case("bearer") {
        return Err(OAuthClientError::InvalidResponse(format!(
            "token endpoint returned unsupported token type `{}`",
            response.token_type
        )));
    }
    let scopes = response
        .scope
        .as_deref()
        .map(|scope| unique_scopes(scope.split_ascii_whitespace()))
        .unwrap_or_else(|| requested_scopes.to_vec());
    Ok(OAuthStoredToken {
        access_token: response.access_token,
        refresh_token: response.refresh_token.or(previous_refresh_token),
        expires_at: response
            .expires_in
            .map(|lifetime| unix_time().saturating_add(lifetime))
            .unwrap_or(u64::MAX),
        scopes,
    })
}

fn token_is_valid(token: &OAuthStoredToken, buffer: Duration) -> bool {
    unix_time().saturating_add(buffer.as_secs()) < token.expires_at
}

fn scopes_are_covered(requested: &[String], granted: &[String]) -> bool {
    requested
        .iter()
        .all(|scope| granted.iter().any(|granted| granted == scope))
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unique_scopes<I, S>(scopes: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut unique = Vec::new();
    for scope in scopes {
        for scope in scope.as_ref().split_ascii_whitespace() {
            if !scope.is_empty() && !unique.iter().any(|existing| existing == scope) {
                unique.push(scope.to_string());
            }
        }
    }
    unique
}

fn random_urlsafe(bytes: usize) -> String {
    let mut value = vec![0_u8; bytes];
    getrandom::fill(&mut value).expect("getrandom failed");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value)
}

fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

async fn prepare_redirect(
    policy: &OAuthRedirectPolicy,
    _state: &str,
) -> Result<(String, Option<oneshot::Receiver<String>>), OAuthClientError> {
    match policy {
        OAuthRedirectPolicy::Fixed { redirect_uri } => Ok((redirect_uri.clone(), None)),
        OAuthRedirectPolicy::Loopback {
            port,
            callback_path,
        } => {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port.unwrap_or(0)))
                .await
                .map_err(|error| OAuthClientError::Redirect(error.to_string()))?;
            let actual_port = listener
                .local_addr()
                .map_err(|error| OAuthClientError::Redirect(error.to_string()))?
                .port();
            let redirect_uri = format!("http://127.0.0.1:{actual_port}{callback_path}");
            let (sender, receiver) = oneshot::channel();
            let callback_base = format!("http://127.0.0.1:{actual_port}");
            tokio::spawn(run_loopback_callback(listener, sender, callback_base));
            Ok((redirect_uri, Some(receiver)))
        }
    }
}

async fn run_loopback_callback(
    listener: tokio::net::TcpListener,
    sender: oneshot::Sender<String>,
    callback_base: String,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let Ok((mut stream, _)) = listener.accept().await else {
        return;
    };
    let mut bytes = vec![0_u8; 8192];
    let Ok(read) = stream.read(&mut bytes).await else {
        return;
    };
    let request = String::from_utf8_lossy(&bytes[..read]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_ascii_whitespace().nth(1));
    let (status, body) = if target.is_some() {
        ("200 OK", "Authorization received. You can close this tab.")
    } else {
        ("400 Bad Request", "Invalid OAuth callback.")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    if let Some(target) = target {
        let _ = sender.send(format!("{callback_base}{target}"));
    }
}

struct ParsedCallback {
    code: String,
    state: String,
    issuer: Option<String>,
}

fn parse_callback_url(callback_url: &str) -> Result<ParsedCallback, OAuthClientError> {
    let url = reqwest::Url::parse(callback_url)
        .map_err(|error| OAuthClientError::InvalidResponse(error.to_string()))?;
    let mut code = None;
    let mut state = None;
    let mut issuer = None;
    let mut error = None;
    let mut error_description = None;
    for (name, value) in url.query_pairs() {
        match name.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "iss" => issuer = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => error_description = Some(value.into_owned()),
            _ => {}
        }
    }
    if let Some(error) = error {
        return Err(OAuthClientError::InvalidResponse(format!(
            "authorization server returned `{error}`{}",
            error_description
                .map(|description| format!(": {description}"))
                .unwrap_or_default()
        )));
    }
    Ok(ParsedCallback {
        code: code.ok_or_else(|| {
            OAuthClientError::InvalidResponse("callback omitted authorization code".to_string())
        })?,
        state: state.ok_or_else(|| {
            OAuthClientError::InvalidResponse("callback omitted state".to_string())
        })?,
        issuer,
    })
}

fn validate_callback_target(
    callback_url: &str,
    redirect_uri: &str,
) -> Result<(), OAuthClientError> {
    let callback = reqwest::Url::parse(callback_url)
        .map_err(|error| OAuthClientError::InvalidResponse(error.to_string()))?;
    let expected = reqwest::Url::parse(redirect_uri)
        .map_err(|error| OAuthClientError::InvalidResponse(error.to_string()))?;
    if callback.scheme() == expected.scheme()
        && callback.host_str() == expected.host_str()
        && callback.port_or_known_default() == expected.port_or_known_default()
        && callback.path() == expected.path()
    {
        Ok(())
    } else {
        Err(OAuthClientError::InvalidResponse(
            "callback URL does not match the configured redirect URI".to_string(),
        ))
    }
}

fn validate_callback_issuer(
    actual: Option<&str>,
    expected: &str,
    required: bool,
) -> Result<(), OAuthClientError> {
    match actual {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => Err(OAuthClientError::InvalidResponse(format!(
            "authorization response issuer mismatch: expected `{expected}`, got `{actual}`"
        ))),
        None if required => Err(OAuthClientError::InvalidResponse(
            "authorization response omitted required `iss`".to_string(),
        )),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::client::OAuthScopeChallenge;

    #[derive(Debug, Clone, Copy)]
    enum RegistrationMode {
        PreRegistered,
        Cimd,
        Dynamic,
        PrivateKeyJwt,
    }

    #[derive(Clone)]
    struct MockOAuthHttp {
        mode: RegistrationMode,
        requests: Arc<Mutex<Vec<OAuthHttpRequest>>>,
        token_requests: Arc<AtomicUsize>,
        expire_initial_token: bool,
    }

    impl MockOAuthHttp {
        fn new(mode: RegistrationMode) -> Self {
            Self {
                mode,
                requests: Arc::new(Mutex::new(Vec::new())),
                token_requests: Arc::new(AtomicUsize::new(0)),
                expire_initial_token: false,
            }
        }

        fn expiring(mut self) -> Self {
            self.expire_initial_token = true;
            self
        }

        async fn requests(&self) -> Vec<OAuthHttpRequest> {
            self.requests.lock().await.clone()
        }

        fn response(status: u16, body: serde_json::Value) -> OAuthHttpResponse {
            OAuthHttpResponse {
                status,
                headers: Vec::new(),
                body: serde_json::to_vec(&body).unwrap(),
            }
        }
    }

    #[async_trait]
    impl OAuthHttpClient for MockOAuthHttp {
        async fn execute(
            &self,
            request: OAuthHttpRequest,
        ) -> Result<OAuthHttpResponse, OAuthClientError> {
            self.requests.lock().await.push(request.clone());
            let path = reqwest::Url::parse(&request.url)
                .unwrap()
                .path()
                .to_string();
            match path.as_str() {
                "/mcp" => Ok(OAuthHttpResponse {
                    status: 401,
                    headers: vec![(
                        "www-authenticate".to_string(),
                        "Bearer resource_metadata=\"https://mcp.example.com/prm\", scope=\"challenge.scope\""
                            .to_string(),
                    )],
                    body: Vec::new(),
                }),
                "/prm" => Ok(Self::response(
                    200,
                    serde_json::json!({
                        "resource": "https://mcp.example.com/mcp",
                        "authorization_servers": ["https://auth.example.com/issuer"],
                        "scopes_supported": ["prm.scope"]
                    }),
                )),
                "/.well-known/oauth-authorization-server/issuer" => {
                    let (cimd, methods) = match self.mode {
                        RegistrationMode::PreRegistered => (false, vec!["client_secret_basic"]),
                        RegistrationMode::Cimd => (true, vec!["none"]),
                        RegistrationMode::Dynamic => (false, vec!["none"]),
                        RegistrationMode::PrivateKeyJwt => (false, vec!["private_key_jwt"]),
                    };
                    Ok(Self::response(
                        200,
                        serde_json::json!({
                            "issuer": "https://auth.example.com/issuer",
                            "authorization_endpoint": "https://auth.example.com/authorize",
                            "token_endpoint": "https://auth.example.com/token",
                            "registration_endpoint": "https://auth.example.com/register",
                            "client_id_metadata_document_supported": cimd,
                            "authorization_response_iss_parameter_supported": true,
                            "code_challenge_methods_supported": ["S256"],
                            "token_endpoint_auth_methods_supported": methods,
                            "grant_types_supported": ["authorization_code", "refresh_token"],
                            "scopes_supported": ["challenge.scope", "prm.scope", "extra.scope", "offline_access"]
                        }),
                    ))
                }
                "/register" => Ok(Self::response(
                    201,
                    serde_json::json!({ "client_id": "dynamic-client" }),
                )),
                "/token" => {
                    let request_number = self.token_requests.fetch_add(1, Ordering::SeqCst);
                    let fields = match &request.body {
                        OAuthHttpBody::Form(fields) => fields,
                        body => panic!("expected form token request, got {body:?}"),
                    };
                    let grant = fields
                        .iter()
                        .find(|(name, _)| name == "grant_type")
                        .map(|(_, value)| value.as_str())
                        .unwrap();
                    let scope = fields
                        .iter()
                        .find(|(name, _)| name == "scope")
                        .map(|(_, value)| value.clone())
                        .unwrap_or_else(|| {
                            if request_number == 0 {
                                "challenge.scope offline_access".to_string()
                            } else {
                                "challenge.scope offline_access extra.scope".to_string()
                            }
                        });
                    if grant == "refresh_token" {
                        Ok(Self::response(
                            200,
                            serde_json::json!({
                                "access_token": "refreshed-token",
                                "token_type": "Bearer",
                                "expires_in": 3600,
                                "scope": scope
                            }),
                        ))
                    } else {
                        Ok(Self::response(
                            200,
                            serde_json::json!({
                                "access_token": format!("access-token-{request_number}"),
                                "token_type": "Bearer",
                                "expires_in": if self.expire_initial_token { 0 } else { 3600 },
                                "refresh_token": "refresh-token",
                                "scope": scope
                            }),
                        ))
                    }
                }
                other => Err(OAuthClientError::Http(format!(
                    "unexpected mock request path `{other}`"
                ))),
            }
        }
    }

    #[derive(Clone, Default)]
    struct AutomaticAuthorizationHandler {
        calls: Arc<AtomicUsize>,
    }

    #[derive(Clone, Default)]
    struct TestAssertionSigner;

    #[async_trait]
    impl OAuthClientAssertionSigner for TestAssertionSigner {
        async fn sign_client_assertion(
            &self,
            request: OAuthClientAssertionRequest,
        ) -> Result<String, OAuthClientError> {
            assert_eq!(request.client_id, "signed-client");
            assert_eq!(request.token_endpoint, "https://auth.example.com/token");
            assert_eq!(
                request.authorization_server_issuer,
                "https://auth.example.com/issuer"
            );
            Ok("signed-client-assertion".to_string())
        }
    }

    #[async_trait]
    impl OAuthAuthorizationHandler for AutomaticAuthorizationHandler {
        async fn authorize(
            &self,
            request: OAuthAuthorizationRequest,
        ) -> Result<OAuthAuthorizationAction, OAuthClientError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let authorization_url = reqwest::Url::parse(&request.authorization_url).unwrap();
            let state = authorization_url
                .query_pairs()
                .find(|(name, _)| name == "state")
                .unwrap()
                .1
                .into_owned();
            let mut callback = reqwest::Url::parse(&request.redirect_uri).unwrap();
            callback
                .query_pairs_mut()
                .append_pair("code", "authorization-code")
                .append_pair("state", &state)
                .append_pair("iss", &request.issuer);
            Ok(OAuthAuthorizationAction::CallbackUrl(callback.to_string()))
        }
    }

    fn dynamic_options() -> OAuthClientRegistrationOptions {
        OAuthClientRegistrationOptions::new()
            .with_client_id_metadata_document("https://client.example.com/metadata.json")
            .with_dynamic_registration(
                super::super::oauth_authcode::OAuthDynamicClientRegistration::native(
                    "test-client",
                    std::iter::empty::<String>(),
                ),
            )
    }

    fn flow_builder(
        http: MockOAuthHttp,
        options: OAuthClientRegistrationOptions,
        handler: AutomaticAuthorizationHandler,
    ) -> OAuthAuthorizationFlowBuilder {
        OAuthAuthorizationFlow::builder("https://mcp.example.com/mcp")
            .http_client(http)
            .redirect_policy(OAuthRedirectPolicy::fixed(
                "http://127.0.0.1:23456/callback",
            ))
            .registration_options(options)
            .authorization_handler(handler)
    }

    #[tokio::test]
    async fn preregistered_flow_binds_resource_and_uses_basic_auth() {
        let http = MockOAuthHttp::new(RegistrationMode::PreRegistered);
        let flow = flow_builder(
            http.clone(),
            OAuthClientRegistrationOptions::new(),
            AutomaticAuthorizationHandler::default(),
        )
        .pre_registered_client("pre:client", Some("pre secret".to_string()))
        .build()
        .unwrap();

        flow.authorize(std::iter::empty::<&str>()).await.unwrap();
        assert_eq!(flow.get_token().await.unwrap(), "access-token-0");
        assert_eq!(
            flow.authorized_scopes().await.unwrap(),
            vec!["challenge.scope", "offline_access"]
        );

        let requests = http.requests().await;
        let probe = requests.first().unwrap();
        assert_eq!(probe.method, OAuthHttpMethod::Post);
        let OAuthHttpBody::Json(probe_body) = &probe.body else {
            panic!("expected JSON MCP probe")
        };
        assert_eq!(probe_body["method"], "tools/list");
        assert!(
            !requests
                .iter()
                .any(|request| request.url.ends_with("/register"))
        );
        let token_request = requests
            .iter()
            .find(|request| request.url.ends_with("/token"))
            .unwrap();
        assert!(token_request.headers.iter().any(|(name, value)| {
            name == "authorization"
                && value
                    == &format!(
                        "Basic {}",
                        base64::engine::general_purpose::STANDARD
                            .encode("pre%3Aclient:pre%20secret")
                    )
        }));
        let OAuthHttpBody::Form(fields) = &token_request.body else {
            panic!("expected token form")
        };
        assert!(fields.iter().any(|field| field
            == &(
                "resource".to_string(),
                "https://mcp.example.com/mcp".to_string()
            )));
    }

    #[tokio::test]
    async fn private_key_jwt_is_used_when_advertised() {
        let http = MockOAuthHttp::new(RegistrationMode::PrivateKeyJwt);
        let options = OAuthClientRegistrationOptions::new().with_pre_registered(
            OAuthClientRegistration::pre_registered(
                "https://auth.example.com/issuer",
                "signed-client",
                None,
            ),
        );
        let flow = flow_builder(
            http.clone(),
            options,
            AutomaticAuthorizationHandler::default(),
        )
        .client_assertion_signer(TestAssertionSigner)
        .build()
        .unwrap();

        flow.authorize(["challenge.scope"]).await.unwrap();
        let requests = http.requests().await;
        let token_request = requests
            .iter()
            .find(|request| request.url.ends_with("/token"))
            .unwrap();
        let OAuthHttpBody::Form(fields) = &token_request.body else {
            panic!("expected token form")
        };
        assert!(fields.iter().any(|field| field
            == &(
                "client_assertion_type".to_string(),
                "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".to_string()
            )));
        assert!(fields.iter().any(|field| field
            == &(
                "client_assertion".to_string(),
                "signed-client-assertion".to_string()
            )));
    }

    #[tokio::test]
    async fn cimd_takes_priority_over_dynamic_registration() {
        let http = MockOAuthHttp::new(RegistrationMode::Cimd);
        let flow = flow_builder(
            http.clone(),
            dynamic_options(),
            AutomaticAuthorizationHandler::default(),
        )
        .build()
        .unwrap();

        flow.authorize(["explicit.scope"]).await.unwrap();
        let requests = http.requests().await;
        assert!(
            !requests
                .iter()
                .any(|request| request.url.ends_with("/register"))
        );
        let authorization = requests
            .iter()
            .find(|request| request.url.ends_with("/token"))
            .unwrap();
        let OAuthHttpBody::Form(fields) = &authorization.body else {
            panic!("expected form")
        };
        assert!(fields.iter().any(|field| field
            == &(
                "client_id".to_string(),
                "https://client.example.com/metadata.json".to_string()
            )));
        assert!(fields.iter().any(|field| field
            == &(
                "resource".to_string(),
                "https://mcp.example.com/mcp".to_string()
            )));
    }

    #[tokio::test]
    async fn dynamic_registration_is_persisted_and_reused() {
        let http = MockOAuthHttp::new(RegistrationMode::Dynamic);
        let registrations = super::super::oauth_authcode::MemoryOAuthClientRegistrationStore::new();

        for _ in 0..2 {
            let flow = flow_builder(
                http.clone(),
                dynamic_options(),
                AutomaticAuthorizationHandler::default(),
            )
            .registration_store(registrations.clone())
            .build()
            .unwrap();
            flow.authorize(["challenge.scope"]).await.unwrap();
        }

        let requests = http.requests().await;
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.ends_with("/register"))
                .count(),
            1
        );
        let registration_request = requests
            .iter()
            .find(|request| request.url.ends_with("/register"))
            .unwrap();
        let OAuthHttpBody::Json(value) = &registration_request.body else {
            panic!("expected registration JSON")
        };
        assert_eq!(value["redirect_uris"][0], "http://127.0.0.1:23456/callback");
    }

    #[tokio::test]
    async fn expired_token_refreshes_and_preserves_binding() {
        let http = MockOAuthHttp::new(RegistrationMode::Dynamic).expiring();
        let flow = flow_builder(
            http.clone(),
            dynamic_options(),
            AutomaticAuthorizationHandler::default(),
        )
        .refresh_buffer(Duration::ZERO)
        .build()
        .unwrap();

        flow.authorize(["challenge.scope"]).await.unwrap();
        assert_eq!(flow.get_token().await.unwrap(), "refreshed-token");
        let requests = http.requests().await;
        let refresh = requests
            .iter()
            .rfind(|request| request.url.ends_with("/token"))
            .unwrap();
        let OAuthHttpBody::Form(fields) = &refresh.body else {
            panic!("expected refresh form")
        };
        assert!(
            fields
                .iter()
                .any(|field| field == &("grant_type".to_string(), "refresh_token".to_string()))
        );
        assert!(fields.iter().any(|field| field
            == &(
                "resource".to_string(),
                "https://mcp.example.com/mcp".to_string()
            )));
    }

    #[tokio::test]
    async fn scope_escalation_reauthorizes_same_provider() {
        let http = MockOAuthHttp::new(RegistrationMode::Dynamic);
        let handler = AutomaticAuthorizationHandler::default();
        let calls = handler.calls.clone();
        let flow = flow_builder(http, dynamic_options(), handler)
            .build()
            .unwrap();
        flow.authorize(std::iter::empty::<&str>()).await.unwrap();

        flow.reauthorize(OAuthScopeEscalationRequest {
            resource: "https://mcp.example.com/mcp".to_string(),
            operation: "tools/call:admin".to_string(),
            challenge: OAuthScopeChallenge {
                required_scopes: vec!["extra.scope".to_string()],
                resource_metadata: Some("https://mcp.example.com/prm".to_string()),
                error_description: None,
            },
            previous_scopes: vec!["challenge.scope".to_string(), "offline_access".to_string()],
            requested_scopes: vec![
                "challenge.scope".to_string(),
                "offline_access".to_string(),
                "extra.scope".to_string(),
            ],
            attempt: 1,
        })
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            flow.authorized_scopes().await.unwrap(),
            vec!["challenge.scope", "offline_access", "extra.scope"]
        );
        assert_eq!(flow.get_token().await.unwrap(), "access-token-1");
    }

    #[tokio::test]
    async fn persisted_pkce_state_can_complete_after_flow_rebuild() {
        let http = MockOAuthHttp::new(RegistrationMode::Dynamic);
        let state_store = MemoryOAuthAuthorizationStateStore::new();
        let token_store = MemoryOAuthTokenStore::new();
        let registrations = super::super::oauth_authcode::MemoryOAuthClientRegistrationStore::new();
        let first = flow_builder(
            http.clone(),
            dynamic_options(),
            AutomaticAuthorizationHandler::default(),
        )
        .state_store(state_store.clone())
        .token_store(token_store.clone())
        .registration_store(registrations.clone())
        .build()
        .unwrap();
        let OAuthAuthorizationStart::Pending(pending) = first.begin(["prm.scope"]).await.unwrap()
        else {
            panic!("expected pending flow")
        };
        let request = pending.request().clone();
        let authorization_url = reqwest::Url::parse(&request.authorization_url).unwrap();
        let state = authorization_url
            .query_pairs()
            .find(|(name, _)| name == "state")
            .unwrap()
            .1
            .into_owned();
        drop(pending);

        let second = flow_builder(
            http,
            dynamic_options(),
            AutomaticAuthorizationHandler::default(),
        )
        .state_store(state_store)
        .token_store(token_store)
        .registration_store(registrations)
        .build()
        .unwrap();
        let mut callback = reqwest::Url::parse(&request.redirect_uri).unwrap();
        callback
            .query_pairs_mut()
            .append_pair("code", "persisted-code")
            .append_pair("state", &state)
            .append_pair("iss", &request.issuer);
        second
            .complete_callback_url(callback.as_str())
            .await
            .unwrap();
        assert_eq!(second.get_token().await.unwrap(), "access-token-0");
    }

    #[test]
    fn token_without_lifetime_remains_valid_until_the_server_rejects_it() {
        let token = token_from_response(
            MockOAuthHttp::response(
                200,
                serde_json::json!({
                    "access_token": "access-token",
                    "token_type": "Bearer"
                }),
            ),
            &["read".to_string()],
            None,
        )
        .unwrap();

        assert_eq!(token.expires_at, u64::MAX);
        assert!(token_is_valid(&token, Duration::from_secs(30)));
    }

    #[test]
    fn token_response_rejects_non_bearer_token_types() {
        let error = token_from_response(
            MockOAuthHttp::response(
                200,
                serde_json::json!({
                    "access_token": "access-token",
                    "token_type": "DPoP"
                }),
            ),
            &[],
            None,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unsupported token type `DPoP`"));
    }

    #[test]
    fn scope_selection_falls_back_from_challenge_to_resource_metadata() {
        let metadata: OAuthAuthorizationServerMetadata =
            serde_json::from_value(serde_json::json!({
                "issuer": "https://auth.example.com/issuer",
                "authorization_endpoint": "https://auth.example.com/authorize",
                "token_endpoint": "https://auth.example.com/token",
                "grant_types_supported": ["authorization_code"],
                "scopes_supported": ["as.scope"]
            }))
            .unwrap();
        let protected_resource: OAuthProtectedResourceMetadata =
            serde_json::from_value(serde_json::json!({
                "resource": "https://mcp.example.com/mcp",
                "authorization_servers": ["https://auth.example.com/issuer"],
                "scopes_supported": ["prm.scope"]
            }))
            .unwrap();
        let mut discovery = FlowDiscovery {
            resource: protected_resource.resource.clone(),
            protected_resource,
            authorization_servers: vec![metadata.clone()],
            challenge: None,
        };

        assert_eq!(select_scopes(&[], &discovery, &metadata), ["prm.scope"]);
        discovery.challenge = Some(OAuthBearerChallenge {
            error: None,
            scopes: vec!["challenge.scope".to_string()],
            resource_metadata: None,
            error_description: None,
        });
        assert_eq!(
            select_scopes(&[], &discovery, &metadata),
            ["challenge.scope"]
        );
        assert_eq!(
            select_scopes(&["explicit.scope".to_string()], &discovery, &metadata),
            ["explicit.scope"]
        );
    }

    #[test]
    fn resource_identifier_may_be_a_canonical_parent_on_the_same_origin() {
        assert!(
            validate_resource_identifier("https://mcp.example.com/mcp", "https://mcp.example.com")
                .is_ok()
        );
        assert!(
            validate_resource_identifier(
                "https://mcp.example.com/tenant/mcp",
                "https://mcp.example.com/tenant"
            )
            .is_ok()
        );
        assert!(
            validate_resource_identifier(
                "https://mcp.example.com/tenant-evil/mcp",
                "https://mcp.example.com/tenant"
            )
            .is_err()
        );
        assert!(
            validate_resource_identifier(
                "https://other.example.com/mcp",
                "https://mcp.example.com"
            )
            .is_err()
        );
    }
}
