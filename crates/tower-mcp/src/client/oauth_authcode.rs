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

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::{Mutex, RwLock, oneshot};

use super::oauth::{
    OAuthBearerChallenge, OAuthClientError, OAuthTokenEndpointAuthMethod, TokenProvider,
};

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

/// OAuth authorization server metadata used by the authorization-code flow.
///
/// This includes the MCP client-registration capability fields in addition to
/// the RFC 8414 endpoints needed by [`OAuthAuthorizationCode`].
#[derive(Debug, Clone, serde::Deserialize)]
pub struct OAuthAuthorizationServerMetadata {
    /// AS issuer identifier (RFC 8414 §2).
    pub issuer: String,
    /// Authorization endpoint.
    pub authorization_endpoint: String,
    /// Token endpoint.
    pub token_endpoint: String,
    /// Dynamic Client Registration endpoint (RFC 7591), when supported.
    pub registration_endpoint: Option<String>,
    /// Whether the AS supports OAuth Client ID Metadata Documents.
    #[serde(default)]
    pub client_id_metadata_document_supported: bool,
    /// RFC 9207 / SEP-2468: AS advertises that it includes `iss` in
    /// authorization responses. Drives the "absent iss is suspicious"
    /// branch of client-side validation.
    #[serde(default)]
    pub authorization_response_iss_parameter_supported: bool,
    /// PKCE challenge methods supported by the authorization endpoint.
    #[serde(default)]
    pub code_challenge_methods_supported: Vec<String>,
    /// Client authentication methods supported by the token endpoint.
    #[serde(default)]
    pub token_endpoint_auth_methods_supported: Vec<String>,
    /// OAuth grant types supported by the authorization server.
    #[serde(default)]
    pub grant_types_supported: Vec<String>,
    /// OAuth scopes advertised by the authorization server.
    #[serde(default)]
    pub scopes_supported: Vec<String>,
}

/// OAuth Protected Resource Metadata used for MCP authorization discovery.
///
/// See the [MCP authorization-server discovery specification](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization/authorization-server-discovery)
/// and [RFC 9728](https://www.rfc-editor.org/rfc/rfc9728).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct OAuthProtectedResourceMetadata {
    /// Canonical resource identifier used in RFC 8707 `resource` parameters.
    pub resource: String,
    /// Authorization-server issuer identifiers accepted by the resource.
    #[serde(default)]
    pub authorization_servers: Vec<String>,
    /// Scopes understood by the protected resource.
    #[serde(default)]
    pub scopes_supported: Vec<String>,
}

/// Complete, validated OAuth discovery result for an MCP protected resource.
#[derive(Debug, Clone)]
pub struct OAuthAuthorizationDiscovery {
    /// Canonical MCP resource URL used for token audience binding.
    pub resource: String,
    /// Validated Protected Resource Metadata for the MCP server.
    pub protected_resource_metadata: OAuthProtectedResourceMetadata,
    /// Every advertised authorization server whose metadata was fetched and
    /// whose issuer exactly matched its advertised identifier.
    pub authorization_servers: Vec<OAuthAuthorizationServerMetadata>,
    /// Bearer challenge returned by the MCP resource, when one was available.
    pub challenge: Option<OAuthBearerChallenge>,
}

impl OAuthAuthorizationDiscovery {
    /// Select a discovered authorization server by exact issuer identifier.
    pub fn authorization_server(&self, issuer: &str) -> Option<&OAuthAuthorizationServerMetadata> {
        self.authorization_servers
            .iter()
            .find(|metadata| metadata.issuer == issuer)
    }
}

/// Discover the authorization server metadata from the MCP server's
/// Protected Resource Metadata (RFC 9728) or directly from well-known.
pub async fn discover_oauth_authorization_server(
    server_url: &str,
    client: &reqwest::Client,
) -> Result<OAuthAuthorizationServerMetadata, OAuthClientError> {
    let discovery = discover_oauth_authorization(server_url, None, client).await?;
    discovery
        .authorization_servers
        .into_iter()
        .next()
        .ok_or_else(|| OAuthClientError::Discovery("no authorization server discovered".into()))
}

/// Probe an MCP resource for its OAuth Bearer challenge.
pub async fn probe_oauth_bearer_challenge(
    resource_url: &str,
    client: &reqwest::Client,
) -> Result<Option<OAuthBearerChallenge>, OAuthClientError> {
    let response = client
        .get(resource_url)
        .send()
        .await
        .map_err(|e| OAuthClientError::Discovery(e.to_string()))?;
    Ok(response
        .headers()
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(OAuthBearerChallenge::from_www_authenticate))
}

/// Discover Protected Resource Metadata and all advertised authorization
/// servers for an MCP resource.
///
/// A challenge-provided `resource_metadata` URL takes precedence. Otherwise
/// discovery tries the final path-aware RFC 9728 location and then the origin
/// root for compatibility. Authorization-server metadata discovery tries the
/// RFC 8414 and OpenID Connect variants and validates exact issuer equality.
pub async fn discover_oauth_authorization(
    server_url: &str,
    challenge: Option<OAuthBearerChallenge>,
    client: &reqwest::Client,
) -> Result<OAuthAuthorizationDiscovery, OAuthClientError> {
    let challenge = match challenge {
        Some(challenge) => Some(challenge),
        None => probe_oauth_bearer_challenge(server_url, client).await?,
    };

    let challenge_metadata_url = challenge
        .as_ref()
        .and_then(|challenge| challenge.resource_metadata.as_ref())
        .cloned();
    let (metadata_url, protected_resource_metadata) = if let Some(url) = challenge_metadata_url {
        let metadata = fetch_json::<OAuthProtectedResourceMetadata>(client, &url).await?;
        (url, metadata)
    } else {
        let mut discovered = None;
        for url in protected_resource_metadata_urls(server_url)? {
            match fetch_json::<OAuthProtectedResourceMetadata>(client, &url).await {
                Ok(metadata) => {
                    discovered = Some((url, metadata));
                    break;
                }
                Err(error) => {
                    tracing::debug!(%url, %error, "OAuth protected-resource metadata candidate failed")
                }
            }
        }
        discovered.ok_or_else(|| {
            OAuthClientError::Discovery(format!(
                "could not discover Protected Resource Metadata for `{server_url}`"
            ))
        })?
    };
    validate_resource_identifier(server_url, &protected_resource_metadata.resource)?;
    if protected_resource_metadata.authorization_servers.is_empty() {
        return Err(OAuthClientError::Discovery(format!(
            "protected resource metadata at `{metadata_url}` omitted authorization_servers"
        )));
    }

    let resource = protected_resource_metadata.resource.clone();
    let issuers = protected_resource_metadata.authorization_servers.clone();

    let mut authorization_servers = Vec::new();
    let mut last_error = None;
    for issuer in issuers {
        match discover_authorization_server_from_issuer(&issuer, client).await {
            Ok(metadata) => authorization_servers.push(metadata),
            Err(error) => last_error = Some(error),
        }
    }
    if authorization_servers.is_empty() {
        return Err(last_error.unwrap_or_else(|| {
            OAuthClientError::Discovery(
                "protected resource advertised no usable authorization server".into(),
            )
        }));
    }

    Ok(OAuthAuthorizationDiscovery {
        resource,
        protected_resource_metadata,
        authorization_servers,
        challenge,
    })
}

async fn discover_authorization_server_from_issuer(
    issuer: &str,
    client: &reqwest::Client,
) -> Result<OAuthAuthorizationServerMetadata, OAuthClientError> {
    let mut last_error = None;
    for url in authorization_server_metadata_urls(issuer)? {
        match fetch_json::<OAuthAuthorizationServerMetadata>(client, &url).await {
            Ok(metadata) => {
                validate_metadata_issuer(&metadata, issuer)?;
                return Ok(metadata);
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
    client: &reqwest::Client,
    url: &str,
) -> Result<T, OAuthClientError> {
    client
        .get(url)
        .send()
        .await
        .map_err(|error| OAuthClientError::Discovery(error.to_string()))?
        .error_for_status()
        .map_err(|error| OAuthClientError::Discovery(error.to_string()))?
        .json()
        .await
        .map_err(|error| OAuthClientError::Discovery(error.to_string()))
}

fn protected_resource_metadata_urls(server_url: &str) -> Result<Vec<String>, OAuthClientError> {
    let parsed = reqwest::Url::parse(server_url)
        .map_err(|error| OAuthClientError::Discovery(error.to_string()))?;
    let origin = parsed.origin().ascii_serialization();
    let path = parsed.path().trim_end_matches('/');
    let mut urls = Vec::new();
    if !path.is_empty() {
        push_unique(
            &mut urls,
            format!("{origin}/.well-known/oauth-protected-resource{path}"),
        );
    }
    push_unique(
        &mut urls,
        format!("{origin}/.well-known/oauth-protected-resource"),
    );
    Ok(urls)
}

fn authorization_server_metadata_urls(issuer: &str) -> Result<Vec<String>, OAuthClientError> {
    let parsed = reqwest::Url::parse(issuer)
        .map_err(|error| OAuthClientError::Discovery(error.to_string()))?;
    let origin = parsed.origin().ascii_serialization();
    let path = parsed.path().trim_end_matches('/');
    let trimmed = issuer.trim_end_matches('/');
    let mut urls = Vec::new();
    if !path.is_empty() {
        push_unique(
            &mut urls,
            format!("{origin}/.well-known/oauth-authorization-server{path}"),
        );
        push_unique(
            &mut urls,
            format!("{origin}/.well-known/openid-configuration{path}"),
        );
        push_unique(
            &mut urls,
            format!("{trimmed}/.well-known/openid-configuration"),
        );
    } else {
        push_unique(
            &mut urls,
            format!("{origin}/.well-known/oauth-authorization-server"),
        );
        push_unique(
            &mut urls,
            format!("{origin}/.well-known/openid-configuration"),
        );
    }
    Ok(urls)
}

fn validate_resource_identifier(server_url: &str, resource: &str) -> Result<(), OAuthClientError> {
    let server = reqwest::Url::parse(server_url)
        .map_err(|error| OAuthClientError::Discovery(error.to_string()))?;
    let metadata = reqwest::Url::parse(resource)
        .map_err(|error| OAuthClientError::Discovery(error.to_string()))?;
    let matches = metadata.fragment().is_none()
        && server.scheme() == metadata.scheme()
        && server.host_str() == metadata.host_str()
        && server.port_or_known_default() == metadata.port_or_known_default()
        && server.path() == metadata.path()
        && server.query() == metadata.query();
    if matches {
        Ok(())
    } else {
        Err(OAuthClientError::Discovery(format!(
            "protected resource metadata mismatch: expected `{server_url}`, got `{resource}`"
        )))
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

/// Validate the metadata issuer against the authorization-server identifier
/// used to construct its well-known URL.
///
/// MCP requires exact string equality here. In particular, trailing slashes
/// are significant and must not be normalized before comparison.
fn validate_metadata_issuer(
    metadata: &OAuthAuthorizationServerMetadata,
    expected: &str,
) -> Result<(), OAuthClientError> {
    if metadata.issuer == expected {
        Ok(())
    } else {
        Err(OAuthClientError::Discovery(format!(
            "authorization server metadata issuer mismatch: expected `{expected}`, got `{}`",
            metadata.issuer
        )))
    }
}

// =============================================================================
// Client registration
// =============================================================================

/// Client-registration mechanism selected for an authorization-code flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OAuthClientRegistrationMethod {
    /// Credentials registered with a specific authorization server in advance.
    PreRegistered,
    /// A portable HTTPS Client ID Metadata Document URL.
    ClientIdMetadataDocument,
    /// Dynamic Client Registration (RFC 7591).
    Dynamic,
}

/// Client credentials selected or created for an authorization-code flow.
///
/// [`bound_issuer()`](Self::bound_issuer) is set for pre-registered and
/// dynamically registered clients. Client ID Metadata Document identifiers are
/// portable across authorization servers and therefore have no issuer binding.
///
/// The serialized representation includes `client_secret` so persistent store
/// implementations can round-trip credentials. Treat it as sensitive data and
/// only serialize it into an appropriately protected secret store.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OAuthClientRegistration {
    client_id: String,
    client_secret: Option<String>,
    method: OAuthClientRegistrationMethod,
    bound_issuer: Option<String>,
}

impl fmt::Debug for OAuthClientRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthClientRegistration")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field("method", &self.method)
            .field("bound_issuer", &self.bound_issuer)
            .finish()
    }
}

impl OAuthClientRegistration {
    /// Create issuer-bound pre-registered client credentials.
    pub fn pre_registered(
        issuer: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: Option<String>,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret,
            method: OAuthClientRegistrationMethod::PreRegistered,
            bound_issuer: Some(issuer.into()),
        }
    }

    /// Restore issuer-bound credentials obtained by Dynamic Client
    /// Registration.
    ///
    /// Applications normally receive this form from
    /// [`resolve_oauth_client_registration_with_store`]. This constructor lets
    /// a persistent [`OAuthClientRegistrationStore`] rebuild a registration
    /// from separately stored fields without relying on a particular
    /// serialization format.
    pub fn dynamically_registered(
        issuer: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: Option<String>,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret,
            method: OAuthClientRegistrationMethod::Dynamic,
            bound_issuer: Some(issuer.into()),
        }
    }

    /// Create a portable Client ID Metadata Document registration.
    pub fn client_id_metadata_document(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: None,
            method: OAuthClientRegistrationMethod::ClientIdMetadataDocument,
            bound_issuer: None,
        }
    }

    /// OAuth client ID.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// OAuth client secret, when the registration issued one.
    pub fn client_secret(&self) -> Option<&str> {
        self.client_secret.as_deref()
    }

    /// Registration mechanism used to obtain the client ID.
    pub fn method(&self) -> OAuthClientRegistrationMethod {
        self.method
    }

    /// Authorization-server issuer to which these credentials are bound.
    ///
    /// This is `None` only for portable Client ID Metadata Document URLs.
    pub fn bound_issuer(&self) -> Option<&str> {
        self.bound_issuer.as_deref()
    }
}

/// Persistent storage for issuer-bound OAuth client registrations.
///
/// Implementations must use the exact validated authorization-server `issuer`
/// string as the key. They must also protect client secrets at rest using an
/// appropriate platform secret store or equivalent controls.
///
/// Only pre-registered and dynamically registered credentials are
/// issuer-bound. Client ID Metadata Document URLs are portable and are not
/// passed to this store by [`resolve_oauth_client_registration_with_store`].
#[async_trait]
pub trait OAuthClientRegistrationStore: Send + Sync {
    /// Load client credentials registered with `issuer`.
    async fn load(&self, issuer: &str)
    -> Result<Option<OAuthClientRegistration>, OAuthClientError>;

    /// Save client credentials under their exact authorization-server issuer.
    async fn save(
        &self,
        issuer: &str,
        registration: &OAuthClientRegistration,
    ) -> Result<(), OAuthClientError>;

    /// Remove client credentials for an authorization-server issuer.
    async fn remove(&self, issuer: &str) -> Result<(), OAuthClientError>;
}

/// Process-local issuer-keyed OAuth client registration store.
///
/// This is useful for applications that only need credentials for the current
/// process and as a reference implementation for persistent secret-store
/// adapters. It does not persist credentials across process restarts.
#[derive(Clone, Default)]
pub struct MemoryOAuthClientRegistrationStore {
    registrations: Arc<RwLock<HashMap<String, OAuthClientRegistration>>>,
}

impl fmt::Debug for MemoryOAuthClientRegistrationStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryOAuthClientRegistrationStore")
            .finish_non_exhaustive()
    }
}

impl MemoryOAuthClientRegistrationStore {
    /// Create an empty registration store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the number of issuer registrations currently stored.
    pub async fn len(&self) -> usize {
        self.registrations.read().await.len()
    }

    /// Return whether the store contains no registrations.
    pub async fn is_empty(&self) -> bool {
        self.registrations.read().await.is_empty()
    }
}

#[async_trait]
impl OAuthClientRegistrationStore for MemoryOAuthClientRegistrationStore {
    async fn load(
        &self,
        issuer: &str,
    ) -> Result<Option<OAuthClientRegistration>, OAuthClientError> {
        Ok(self.registrations.read().await.get(issuer).cloned())
    }

    async fn save(
        &self,
        issuer: &str,
        registration: &OAuthClientRegistration,
    ) -> Result<(), OAuthClientError> {
        self.registrations
            .write()
            .await
            .insert(issuer.to_string(), registration.clone());
        Ok(())
    }

    async fn remove(&self, issuer: &str) -> Result<(), OAuthClientError> {
        self.registrations.write().await.remove(issuer);
        Ok(())
    }
}

/// OAuth application type sent during Dynamic Client Registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OAuthApplicationType {
    /// Desktop, mobile, CLI, or locally hosted application.
    Native,
    /// Remotely hosted browser application.
    Web,
}

/// Dynamic Client Registration request metadata.
///
/// Use [`native()`](Self::native) for desktop, mobile, CLI, and localhost
/// clients, as required by the MCP 2026-07-28 authorization specification.
#[derive(Debug, Clone, serde::Serialize)]
#[non_exhaustive]
pub struct OAuthDynamicClientRegistration {
    /// Human-readable client name.
    pub client_name: String,
    /// OIDC application type. MCP clients must choose this explicitly.
    pub application_type: OAuthApplicationType,
    /// Allowed redirect URIs.
    pub redirect_uris: Vec<String>,
    /// Requested OAuth grant types.
    pub grant_types: Vec<String>,
    /// Requested OAuth response types.
    pub response_types: Vec<String>,
    /// Token endpoint authentication method.
    pub token_endpoint_auth_method: String,
}

impl OAuthDynamicClientRegistration {
    /// Create registration metadata for a native application.
    pub fn native(
        client_name: impl Into<String>,
        redirect_uris: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            client_name: client_name.into(),
            application_type: OAuthApplicationType::Native,
            redirect_uris: redirect_uris.into_iter().map(Into::into).collect(),
            grant_types: vec!["authorization_code".to_string()],
            response_types: vec!["code".to_string()],
            token_endpoint_auth_method: "none".to_string(),
        }
    }

    /// Create registration metadata for a web application.
    pub fn web(
        client_name: impl Into<String>,
        redirect_uris: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            application_type: OAuthApplicationType::Web,
            ..Self::native(client_name, redirect_uris)
        }
    }

    /// Override the requested grant types.
    pub fn grant_types(mut self, grant_types: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.grant_types = grant_types.into_iter().map(Into::into).collect();
        self
    }

    /// Override the token endpoint authentication method.
    pub fn token_endpoint_auth_method(mut self, method: impl Into<String>) -> Self {
        self.token_endpoint_auth_method = method.into();
        self
    }
}

/// Registration mechanisms available to an OAuth authorization-code client.
///
/// [`resolve_oauth_client_registration`] applies the MCP 2026-07-28 priority
/// order: pre-registration, Client ID Metadata Documents, then Dynamic Client
/// Registration.
#[derive(Debug, Clone, Default)]
pub struct OAuthClientRegistrationOptions {
    /// Issuer-bound credentials registered ahead of time.
    pub pre_registered: Option<OAuthClientRegistration>,
    /// HTTPS URL of the client's metadata document.
    pub client_id_metadata_document: Option<String>,
    /// Metadata to send when falling back to Dynamic Client Registration.
    pub dynamic_registration: Option<OAuthDynamicClientRegistration>,
}

impl OAuthClientRegistrationOptions {
    /// Create an empty set of registration options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Supply issuer-bound pre-registered credentials.
    pub fn with_pre_registered(mut self, registration: OAuthClientRegistration) -> Self {
        self.pre_registered = Some(registration);
        self
    }

    /// Supply the client's HTTPS Client ID Metadata Document URL.
    pub fn with_client_id_metadata_document(mut self, client_id: impl Into<String>) -> Self {
        self.client_id_metadata_document = Some(client_id.into());
        self
    }

    /// Enable Dynamic Client Registration as a fallback.
    pub fn with_dynamic_registration(
        mut self,
        registration: OAuthDynamicClientRegistration,
    ) -> Self {
        self.dynamic_registration = Some(registration);
        self
    }
}

#[derive(Debug, serde::Deserialize)]
struct DynamicClientRegistrationResponse {
    client_id: String,
    client_secret: Option<String>,
}

/// Select and, when necessary, perform OAuth client registration.
///
/// Pre-registered credentials are accepted only when their issuer binding
/// exactly matches `metadata.issuer`. CIMD URLs must use HTTPS and contain a
/// non-root path. If no configured mechanism is supported, the returned error
/// tells the caller to prompt the user for client information.
pub async fn resolve_oauth_client_registration(
    client: &reqwest::Client,
    metadata: &OAuthAuthorizationServerMetadata,
    options: &OAuthClientRegistrationOptions,
) -> Result<OAuthClientRegistration, OAuthClientError> {
    resolve_oauth_client_registration_inner(client, metadata, options, None).await
}

/// Select or create an OAuth client registration with issuer-keyed
/// persistence.
///
/// The resolver applies the same priority order as
/// [`resolve_oauth_client_registration`]. On the Dynamic Client Registration
/// path, it first reuses credentials stored under the metadata's exact
/// validated `issuer`; newly registered credentials are saved under that key.
///
/// If protected-resource metadata later selects a different authorization
/// server, the different issuer key guarantees that old credentials are not
/// reused. The resolver performs a new Dynamic Client Registration with the
/// new server instead. Stored credentials for the previous issuer are retained
/// because another resource may still use that authorization server.
pub async fn resolve_oauth_client_registration_with_store(
    client: &reqwest::Client,
    metadata: &OAuthAuthorizationServerMetadata,
    options: &OAuthClientRegistrationOptions,
    store: &dyn OAuthClientRegistrationStore,
) -> Result<OAuthClientRegistration, OAuthClientError> {
    resolve_oauth_client_registration_inner(client, metadata, options, Some(store)).await
}

async fn resolve_oauth_client_registration_inner(
    client: &reqwest::Client,
    metadata: &OAuthAuthorizationServerMetadata,
    options: &OAuthClientRegistrationOptions,
    store: Option<&dyn OAuthClientRegistrationStore>,
) -> Result<OAuthClientRegistration, OAuthClientError> {
    if let Some(registration) = &options.pre_registered {
        if registration.method != OAuthClientRegistrationMethod::PreRegistered {
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
        validate_client_id_metadata_document_url(client_id)?;
        return Ok(OAuthClientRegistration {
            client_id: client_id.clone(),
            client_secret: None,
            method: OAuthClientRegistrationMethod::ClientIdMetadataDocument,
            bound_issuer: None,
        });
    }

    if options.dynamic_registration.is_some()
        && let Some(store) = store
        && let Some(registration) = store.load(&metadata.issuer).await?
    {
        validate_stored_dynamic_registration(&registration, &metadata.issuer)?;
        return Ok(registration);
    }

    if let (Some(endpoint), Some(request)) = (
        metadata.registration_endpoint.as_deref(),
        options.dynamic_registration.as_ref(),
    ) {
        if request.redirect_uris.is_empty() {
            return Err(OAuthClientError::BuildError(
                "dynamic registration requires at least one redirect URI".to_string(),
            ));
        }

        let response = client
            .post(endpoint)
            .json(request)
            .send()
            .await
            .map_err(|error| OAuthClientError::Registration(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body: String = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(1024)
                .collect();
            return Err(OAuthClientError::Registration(format!(
                "dynamic client registration failed with {status}: {body}"
            )));
        }
        let response: DynamicClientRegistrationResponse = response
            .json()
            .await
            .map_err(|error| OAuthClientError::Registration(error.to_string()))?;
        let registration = OAuthClientRegistration::dynamically_registered(
            metadata.issuer.clone(),
            response.client_id,
            response.client_secret,
        );
        if let Some(store) = store {
            store.save(&metadata.issuer, &registration).await?;
        }
        return Ok(registration);
    }

    Err(OAuthClientError::BuildError(
        "authorization server supports none of the configured client registration mechanisms; \
         prompt the user for pre-registered client information"
            .to_string(),
    ))
}

fn validate_stored_dynamic_registration(
    registration: &OAuthClientRegistration,
    issuer: &str,
) -> Result<(), OAuthClientError> {
    if registration.method() != OAuthClientRegistrationMethod::Dynamic {
        return Err(OAuthClientError::CredentialStore(format!(
            "stored registration for issuer `{issuer}` uses {:?}, expected dynamic registration",
            registration.method()
        )));
    }
    if registration.bound_issuer() != Some(issuer) {
        return Err(OAuthClientError::CredentialStore(format!(
            "stored registration is bound to issuer {:?}, not `{issuer}`",
            registration.bound_issuer()
        )));
    }
    Ok(())
}

fn validate_client_id_metadata_document_url(client_id: &str) -> Result<(), OAuthClientError> {
    let url = reqwest::Url::parse(client_id).map_err(|error| {
        OAuthClientError::BuildError(format!(
            "invalid Client ID Metadata Document URL `{client_id}`: {error}"
        ))
    })?;
    if url.scheme() != "https" || url.path() == "/" {
        return Err(OAuthClientError::BuildError(format!(
            "Client ID Metadata Document URL `{client_id}` must use HTTPS and contain a path"
        )));
    }
    Ok(())
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
    /// Authentication method selected from authorization-server metadata.
    token_endpoint_auth_method: OAuthTokenEndpointAuthMethod,
    /// Canonical protected-resource identifier (RFC 8707).
    resource: String,
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
    /// SEP-2468 / RFC 9207: expected `iss` value, recorded at start time
    /// from AS metadata. Used to validate the authorization response's
    /// `iss` parameter against the originating server.
    expected_issuer: Option<String>,
    /// SEP-2468: whether the AS advertises iss-in-response support. When
    /// `true`, a missing `iss` in the callback is grounds for rejection
    /// per RFC 9207 §2.4. When `false`, missing `iss` is tolerated.
    iss_required: bool,
}

#[derive(Debug)]
struct CallbackResult {
    code: String,
    #[allow(dead_code)]
    state: String,
    /// SEP-2468: `iss` parameter from the authorization response, if the
    /// AS included it. Validated against `expected_issuer`.
    iss: Option<String>,
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
        mut config: OAuthAuthCodeConfig,
    ) -> Result<Self, OAuthClientError> {
        let client = config.http_client.take().unwrap_or_default();

        // Discover the protected resource and every advertised authorization
        // server. Applications can pin an issuer when the PRM advertises more
        // than one; otherwise the resource's preference order is retained.
        let discovery =
            discover_oauth_authorization(server_url, config.challenge.take(), &client).await?;
        let metadata = match config.preferred_authorization_server.take() {
            Some(issuer) => discovery
                .authorization_server(&issuer)
                .cloned()
                .ok_or_else(|| {
                    OAuthClientError::Discovery(format!(
                        "preferred authorization server `{issuer}` was not advertised by the resource"
                    ))
                })?,
            None => discovery
                .authorization_servers
                .first()
                .cloned()
                .ok_or_else(|| OAuthClientError::Discovery("no authorization server discovered".into()))?,
        };
        require_s256(&metadata)?;
        let token_endpoint_auth_method = OAuthTokenEndpointAuthMethod::select(
            &metadata.token_endpoint_auth_methods_supported,
            config.client_secret.is_some(),
        )?;

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
        let scope_str = if !scopes.is_empty() {
            Some(scopes.join(" "))
        } else if let Some(challenge) = &discovery.challenge
            && !challenge.scopes.is_empty()
        {
            Some(challenge.scopes.join(" "))
        } else {
            (!discovery
                .protected_resource_metadata
                .scopes_supported
                .is_empty())
            .then(|| {
                discovery
                    .protected_resource_metadata
                    .scopes_supported
                    .join(" ")
            })
        };

        let client_id = config.client_id.unwrap_or_else(|| "tower-mcp".to_string());
        let mut auth_url = reqwest::Url::parse(&metadata.authorization_endpoint)
            .map_err(|error| OAuthClientError::Discovery(error.to_string()))?;
        {
            let mut query = auth_url.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", &client_id)
                .append_pair("redirect_uri", &redirect_uri)
                .append_pair("state", &state)
                .append_pair("code_challenge", &code_challenge)
                .append_pair("code_challenge_method", "S256");
            if let Some(scopes) = &scope_str {
                query.append_pair("scope", scopes);
            }
            query.append_pair("resource", &discovery.resource);
        }

        Ok(Self {
            inner: Arc::new(OAuthAuthCodeInner {
                authorization_url: auth_url.into(),
                token_endpoint: metadata.token_endpoint,
                client_id,
                client_secret: config.client_secret,
                token_endpoint_auth_method,
                resource: discovery.resource,
                code_verifier,
                state,
                redirect_uri,
                scopes: scope_str,
                refresh_buffer: config.refresh_buffer,
                client,
                cache: RwLock::new(None),
                callback_rx: Mutex::new(Some(callback_rx)),
                _callback_task: callback_task,
                expected_issuer: Some(metadata.issuer),
                iss_required: metadata.authorization_response_iss_parameter_supported,
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

        // SEP-2468: validate `iss` against expected issuer recorded from AS
        // metadata at flow start. Mismatch (or missing-when-required) aborts
        // the flow per RFC 9207 §2.4 to defend against mix-up attacks.
        validate_iss(
            result.iss.as_deref(),
            self.inner.expected_issuer.as_deref(),
            self.inner.iss_required,
        )
        .map_err(OAuthClientError::InvalidResponse)?;

        // Exchange code for tokens
        let token = self.exchange_code(&result.code).await?;
        *self.inner.cache.write().await = Some(token);

        Ok(())
    }

    /// Exchange an authorization code for tokens.
    async fn exchange_code(&self, code: &str) -> Result<CachedAuthCodeToken, OAuthClientError> {
        let response = send_token_request(
            &self.inner.client,
            &self.inner.token_endpoint,
            vec![
                ("grant_type", "authorization_code".to_string()),
                ("code", code.to_string()),
                ("redirect_uri", self.inner.redirect_uri.clone()),
                ("code_verifier", self.inner.code_verifier.clone()),
                ("resource", self.inner.resource.clone()),
            ],
            self.inner.token_endpoint_auth_method,
            &self.inner.client_id,
            self.inner.client_secret.as_deref(),
        )
        .await?;

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
        let mut params = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token.to_string()),
            ("resource", self.inner.resource.clone()),
        ];
        if let Some(ref scopes) = self.inner.scopes {
            params.push(("scope", scopes.clone()));
        }

        let response = send_token_request(
            &self.inner.client,
            &self.inner.token_endpoint,
            params,
            self.inner.token_endpoint_auth_method,
            &self.inner.client_id,
            self.inner.client_secret.as_deref(),
        )
        .await
        .map_err(|error| OAuthClientError::TokenRequest(format!("Refresh failed: {error}")))?;

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

fn require_s256(metadata: &OAuthAuthorizationServerMetadata) -> Result<(), OAuthClientError> {
    if metadata
        .code_challenge_methods_supported
        .iter()
        .any(|method| method == "S256")
    {
        Ok(())
    } else {
        Err(OAuthClientError::Discovery(format!(
            "authorization server `{}` does not advertise PKCE S256 support",
            metadata.issuer
        )))
    }
}

async fn send_token_request(
    client: &reqwest::Client,
    token_endpoint: &str,
    mut params: Vec<(&'static str, String)>,
    method: OAuthTokenEndpointAuthMethod,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<reqwest::Response, OAuthClientError> {
    let mut request = client.post(token_endpoint);
    match method {
        OAuthTokenEndpointAuthMethod::None => {
            params.push(("client_id", client_id.to_string()));
        }
        OAuthTokenEndpointAuthMethod::ClientSecretBasic => {
            let secret = client_secret.ok_or_else(|| {
                OAuthClientError::BuildError(
                    "client_secret_basic requires a client secret".to_string(),
                )
            })?;
            request = request.basic_auth(client_id, Some(secret));
        }
        OAuthTokenEndpointAuthMethod::ClientSecretPost => {
            let secret = client_secret.ok_or_else(|| {
                OAuthClientError::BuildError(
                    "client_secret_post requires a client secret".to_string(),
                )
            })?;
            params.push(("client_id", client_id.to_string()));
            params.push(("client_secret", secret.to_string()));
        }
        OAuthTokenEndpointAuthMethod::PrivateKeyJwt => {
            return Err(OAuthClientError::BuildError(
                "private_key_jwt requires OAuthAuthorizationFlow with a client assertion signer"
                    .to_string(),
            ));
        }
    }
    request
        .form(&params)
        .send()
        .await
        .map_err(|error| OAuthClientError::TokenRequest(error.to_string()))
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
    /// Bearer challenge already obtained from the protected resource.
    ///
    /// When omitted, the client probes `server_url` before falling back to
    /// well-known discovery.
    pub challenge: Option<OAuthBearerChallenge>,
    /// Exact issuer to select when Protected Resource Metadata advertises
    /// multiple authorization servers. The first advertised server is used
    /// when this is omitted.
    pub preferred_authorization_server: Option<String>,
}

impl Default for OAuthAuthCodeConfig {
    fn default() -> Self {
        Self {
            client_id: None,
            client_secret: None,
            callback_port: None,
            refresh_buffer: Duration::from_secs(30),
            http_client: None,
            challenge: None,
            preferred_authorization_server: None,
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
    let mut iss = None;

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
            // SEP-2468 / RFC 9207
            "iss" => iss = Some(decoded),
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

    Ok(CallbackResult { code, state, iss })
}

/// SEP-2468 / RFC 9207 §2.4: validate the authorization response's `iss`
/// parameter against the expected issuer recorded at flow start.
///
/// Rules:
/// - If the AS advertised support (`iss_required = true`) and the
///   callback omits `iss`, REJECT -- the AS promised it would send one.
/// - If `iss` is present, it MUST equal `expected` exactly (simple
///   string compare per the SEP).
/// - If `iss` is absent and the AS did not advertise support, accept.
///   This is the SEP's "comparison instead of discard" rule for legacy
///   AS that have not yet started emitting `iss`.
///
/// Returns Err with a description suitable for surfacing to the user.
fn validate_iss(
    iss: Option<&str>,
    expected: Option<&str>,
    iss_required: bool,
) -> Result<(), String> {
    match (iss, expected, iss_required) {
        (Some(received), Some(want), _) => {
            if received == want {
                Ok(())
            } else {
                Err(format!(
                    "Issuer mismatch (SEP-2468): expected `{}`, got `{}`",
                    want, received
                ))
            }
        }
        (Some(_received), None, _) => {
            // Server sent iss but we never recorded an expected value
            // (AS metadata lacked an `issuer` field). Don't have a baseline
            // to compare against; accept rather than fail-open, but the
            // AS metadata is malformed per RFC 8414.
            Ok(())
        }
        (None, _, true) => Err(
            "Authorization response missing `iss` (SEP-2468): the AS advertises \
             authorization_response_iss_parameter_supported but did not include iss"
                .to_string(),
        ),
        (None, _, false) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn spawn_discovery_server() -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server_base = base.clone();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0_u8; 1024];
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    bytes.extend_from_slice(&chunk[..read]);
                    if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&bytes);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_ascii_whitespace().nth(1))
                    .unwrap()
                    .to_string();
                requests.push(path.clone());

                let (status, extra_headers, body) = match path.as_str() {
                    "/mcp" => (
                        "401 Unauthorized",
                        format!(
                            "WWW-Authenticate: Bearer resource_metadata=\"{server_base}/metadata\", scope=\"challenge.scope\"\r\n"
                        ),
                        String::new(),
                    ),
                    "/metadata" => (
                        "200 OK",
                        String::new(),
                        serde_json::json!({
                            "resource": format!("{server_base}/mcp"),
                            "authorization_servers": [
                                format!("{server_base}/auth-a"),
                                format!("{server_base}/auth-b")
                            ],
                            "scopes_supported": ["metadata.scope"]
                        })
                        .to_string(),
                    ),
                    "/.well-known/oauth-authorization-server/auth-a" => (
                        "200 OK",
                        String::new(),
                        authorization_metadata_json(&server_base, "auth-a"),
                    ),
                    "/.well-known/oauth-authorization-server/auth-b" => (
                        "200 OK",
                        String::new(),
                        authorization_metadata_json(&server_base, "auth-b"),
                    ),
                    other => panic!("unexpected request path: {other}"),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{extra_headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (base, task)
    }

    fn authorization_metadata_json(base: &str, name: &str) -> String {
        let issuer = format!("{base}/{name}");
        serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{base}/{name}/authorize"),
            "token_endpoint": format!("{base}/{name}/token"),
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"]
        })
        .to_string()
    }

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
    fn final_well_known_urls_are_path_aware() {
        assert_eq!(
            protected_resource_metadata_urls("https://mcp.example.com/team/mcp").unwrap(),
            vec![
                "https://mcp.example.com/.well-known/oauth-protected-resource/team/mcp",
                "https://mcp.example.com/.well-known/oauth-protected-resource",
            ]
        );
        let urls = authorization_server_metadata_urls("https://auth.example.com/tenant").unwrap();
        assert_eq!(
            urls[0],
            "https://auth.example.com/.well-known/oauth-authorization-server/tenant"
        );
        assert_eq!(
            urls[1],
            "https://auth.example.com/.well-known/openid-configuration/tenant"
        );
        assert!(
            urls.contains(
                &"https://auth.example.com/tenant/.well-known/openid-configuration".into()
            )
        );
    }

    #[tokio::test]
    async fn public_discovery_honors_challenge_and_exposes_all_servers() {
        let (base, server) = spawn_discovery_server().await;
        let resource = format!("{base}/mcp");
        let discovery = discover_oauth_authorization(&resource, None, &reqwest::Client::new())
            .await
            .unwrap();

        assert_eq!(discovery.resource, resource);
        assert_eq!(discovery.authorization_servers.len(), 2);
        assert!(
            discovery
                .authorization_server(&format!("{base}/auth-b"))
                .is_some()
        );
        assert_eq!(discovery.challenge.unwrap().scopes, vec!["challenge.scope"]);
        assert_eq!(
            server.await.unwrap(),
            vec![
                "/mcp",
                "/metadata",
                "/.well-known/oauth-authorization-server/auth-a",
                "/.well-known/oauth-authorization-server/auth-b",
            ]
        );
    }

    #[tokio::test]
    async fn authorization_flow_selects_issuer_and_binds_resource() {
        let (base, server) = spawn_discovery_server().await;
        let resource = format!("{base}/mcp");
        let provider = OAuthAuthorizationCode::start_with_config(
            &resource,
            &[],
            OAuthAuthCodeConfig {
                client_id: Some("public-client".into()),
                preferred_authorization_server: Some(format!("{base}/auth-b")),
                ..OAuthAuthCodeConfig::default()
            },
        )
        .await
        .unwrap();

        let authorization_url = reqwest::Url::parse(provider.authorization_url()).unwrap();
        assert_eq!(authorization_url.path(), "/auth-b/authorize");
        let parameters: HashMap<_, _> = authorization_url.query_pairs().into_owned().collect();
        assert_eq!(parameters.get("resource"), Some(&resource));
        assert_eq!(
            parameters.get("scope").map(String::as_str),
            Some("challenge.scope")
        );
        assert_eq!(
            parameters.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn token_request_uses_metadata_selected_basic_auth_and_resource() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0_u8; 1024];
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&chunk[..read]);
                if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]).to_string();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while bytes.len() < header_end + content_length {
                let mut chunk = [0_u8; 1024];
                let read = stream.read(&mut chunk).await.unwrap();
                bytes.extend_from_slice(&chunk[..read]);
            }
            let body = String::from_utf8_lossy(&bytes[header_end..header_end + content_length])
                .to_string();
            let response_body = r#"{"access_token":"token","token_type":"Bearer"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            (headers, body)
        });

        let response = send_token_request(
            &reqwest::Client::new(),
            &endpoint,
            vec![
                ("grant_type", "authorization_code".into()),
                ("resource", "https://mcp.example.com/team/mcp".into()),
            ],
            OAuthTokenEndpointAuthMethod::ClientSecretBasic,
            "client id",
            Some("client secret"),
        )
        .await
        .unwrap();
        assert!(response.status().is_success());
        let (headers, body) = server.await.unwrap();
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: basic ")
        );
        assert!(body.contains("resource=https%3A%2F%2Fmcp.example.com%2Fteam%2Fmcp"));
        assert!(!body.contains("client_secret"));
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

    // =========================================================================
    // SEP-2468 / RFC 9207 -- iss parameter validation
    // =========================================================================

    #[test]
    fn parse_callback_extracts_iss_when_present() {
        let result = parse_callback_query(
            "/callback?code=abc&state=s&iss=https%3A%2F%2Fauth.example.com",
            "s",
        )
        .unwrap();
        assert_eq!(result.iss.as_deref(), Some("https://auth.example.com"));
    }

    #[test]
    fn parse_callback_iss_is_none_when_absent() {
        let result = parse_callback_query("/callback?code=abc&state=s", "s").unwrap();
        assert!(result.iss.is_none());
    }

    #[test]
    fn validate_iss_accepts_exact_match() {
        let expected = Some("https://auth.example.com");
        assert!(validate_iss(Some("https://auth.example.com"), expected, true).is_ok());
        assert!(validate_iss(Some("https://auth.example.com"), expected, false).is_ok());
    }

    #[test]
    fn validate_iss_rejects_mismatch_regardless_of_required() {
        let expected = Some("https://auth.example.com");
        let bad = Some("https://evil.example.com");
        for required in [true, false] {
            let err = validate_iss(bad, expected, required).unwrap_err();
            assert!(
                err.contains("Issuer mismatch"),
                "should reject mismatch (required={required}), got: {err}"
            );
        }
    }

    #[test]
    fn validate_iss_rejects_missing_when_as_advertises_support() {
        // SEP-2468 / RFC 9207 §2.4: AS advertised iss support but did not send.
        let err = validate_iss(None, Some("https://auth.example.com"), true).unwrap_err();
        assert!(err.contains("missing `iss`"), "got: {err}");
    }

    #[test]
    fn validate_iss_accepts_missing_when_as_does_not_advertise_support() {
        // Tolerance window: AS predates the iss-emission convention. Accept
        // rather than reject so we don't break legacy flows.
        assert!(validate_iss(None, Some("https://auth.example.com"), false).is_ok());
    }

    #[test]
    fn validate_iss_accepts_when_no_expected_recorded() {
        // AS metadata omitted issuer; we have no baseline to compare against.
        // Accept rather than fail-open, but the AS metadata is non-compliant.
        assert!(validate_iss(Some("https://auth.example.com"), None, false).is_ok());
        assert!(validate_iss(None, None, false).is_ok());
    }

    #[test]
    fn validate_metadata_issuer_accepts_exact_match() {
        let metadata = authorization_server_metadata("https://auth.example.com");
        assert!(validate_metadata_issuer(&metadata, "https://auth.example.com").is_ok());
    }

    #[test]
    fn validate_metadata_issuer_rejects_trailing_slash_mismatch() {
        let metadata = authorization_server_metadata("https://auth.example.com/");
        let err = validate_metadata_issuer(&metadata, "https://auth.example.com").unwrap_err();
        assert!(err.to_string().contains("issuer mismatch"), "got: {err}");
    }

    #[test]
    fn authorization_code_flow_requires_advertised_s256() {
        let mut metadata = authorization_server_metadata("https://auth.example.com");
        metadata.code_challenge_methods_supported.clear();

        let error = require_s256(&metadata).unwrap_err();
        assert!(error.to_string().contains("PKCE S256"));
    }

    #[tokio::test]
    async fn registration_prefers_pre_registered_credentials() {
        let mut metadata = authorization_server_metadata("https://auth.example.com");
        metadata.client_id_metadata_document_supported = true;
        metadata.registration_endpoint = Some("http://127.0.0.1:9/register".to_string());
        let pre_registered = OAuthClientRegistration::pre_registered(
            "https://auth.example.com",
            "configured-client",
            Some("secret".to_string()),
        );
        let options = OAuthClientRegistrationOptions::new()
            .with_pre_registered(pre_registered)
            .with_client_id_metadata_document("https://client.example.com/client.json")
            .with_dynamic_registration(OAuthDynamicClientRegistration::native(
                "test-client",
                ["http://127.0.0.1/callback"],
            ));

        let registration =
            resolve_oauth_client_registration(&reqwest::Client::new(), &metadata, &options)
                .await
                .unwrap();

        assert_eq!(
            registration.method(),
            OAuthClientRegistrationMethod::PreRegistered
        );
        assert_eq!(registration.client_id(), "configured-client");
        assert_eq!(registration.client_secret(), Some("secret"));
        assert_eq!(
            registration.bound_issuer(),
            Some("https://auth.example.com")
        );
    }

    #[tokio::test]
    async fn registration_rejects_pre_registered_issuer_mismatch() {
        let metadata = authorization_server_metadata("https://new-auth.example.com");
        let options = OAuthClientRegistrationOptions::new().with_pre_registered(
            OAuthClientRegistration::pre_registered(
                "https://old-auth.example.com",
                "configured-client",
                None,
            ),
        );

        let error = resolve_oauth_client_registration(&reqwest::Client::new(), &metadata, &options)
            .await
            .unwrap_err();

        assert!(
            error.to_string().contains("bound to issuer"),
            "got: {error}"
        );
    }

    #[tokio::test]
    async fn registration_prefers_cimd_over_dynamic_registration() {
        let mut metadata = authorization_server_metadata("https://auth.example.com");
        metadata.client_id_metadata_document_supported = true;
        metadata.registration_endpoint = Some("http://127.0.0.1:9/register".to_string());
        let options = OAuthClientRegistrationOptions::new()
            .with_client_id_metadata_document("https://client.example.com/client.json")
            .with_dynamic_registration(OAuthDynamicClientRegistration::native(
                "test-client",
                ["http://127.0.0.1/callback"],
            ));

        let registration =
            resolve_oauth_client_registration(&reqwest::Client::new(), &metadata, &options)
                .await
                .unwrap();

        assert_eq!(
            registration.method(),
            OAuthClientRegistrationMethod::ClientIdMetadataDocument
        );
        assert_eq!(
            registration.client_id(),
            "https://client.example.com/client.json"
        );
        assert_eq!(registration.bound_issuer(), None);
    }

    #[tokio::test]
    async fn registration_rejects_invalid_cimd_url() {
        let mut metadata = authorization_server_metadata("https://auth.example.com");
        metadata.client_id_metadata_document_supported = true;
        let options = OAuthClientRegistrationOptions::new()
            .with_client_id_metadata_document("http://client.example.com/client.json");

        let error = resolve_oauth_client_registration(&reqwest::Client::new(), &metadata, &options)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("must use HTTPS"), "got: {error}");
    }

    #[tokio::test]
    async fn registration_falls_back_to_native_dcr_and_binds_issuer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, request_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0u8; 1024];
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&chunk[..read]);
                if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while bytes.len() < header_end + content_length {
                let mut chunk = [0u8; 1024];
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&chunk[..read]);
            }
            let body: serde_json::Value =
                serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap();
            request_tx.send(body).unwrap();

            let response_body = r#"{"client_id":"dynamic-client","client_secret":"secret"}"#;
            let response = format!(
                "HTTP/1.1 201 Created\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let mut metadata = authorization_server_metadata("https://auth.example.com");
        metadata.registration_endpoint = Some(format!("http://{address}/register"));
        let options = OAuthClientRegistrationOptions::new().with_dynamic_registration(
            OAuthDynamicClientRegistration::native("test-client", ["http://127.0.0.1/callback"])
                .grant_types(["authorization_code", "refresh_token"])
                .token_endpoint_auth_method("client_secret_basic"),
        );

        let registration =
            resolve_oauth_client_registration(&reqwest::Client::new(), &metadata, &options)
                .await
                .unwrap();
        let request = request_rx.await.unwrap();
        server.await.unwrap();

        assert_eq!(
            registration.method(),
            OAuthClientRegistrationMethod::Dynamic
        );
        assert_eq!(registration.client_id(), "dynamic-client");
        assert_eq!(registration.client_secret(), Some("secret"));
        assert_eq!(
            registration.bound_issuer(),
            Some("https://auth.example.com")
        );
        assert_eq!(request["application_type"], "native");
        assert_eq!(request["grant_types"][1], "refresh_token");
        assert_eq!(request["token_endpoint_auth_method"], "client_secret_basic");
    }

    #[test]
    fn registration_credentials_round_trip_for_persistent_stores() {
        let registration = OAuthClientRegistration::dynamically_registered(
            "https://auth.example.com",
            "dynamic-client",
            Some("stored-secret".to_string()),
        );

        let json = serde_json::to_string(&registration).unwrap();
        let restored: OAuthClientRegistration = serde_json::from_str(&json).unwrap();

        assert_eq!(restored, registration);
        assert!(!format!("{restored:?}").contains("stored-secret"));
    }

    #[tokio::test]
    async fn stored_dynamic_registration_is_reused_for_exact_issuer() {
        let issuer = "https://auth.example.com";
        let registration = OAuthClientRegistration {
            client_id: "stored-client".to_string(),
            client_secret: Some("stored-secret".to_string()),
            method: OAuthClientRegistrationMethod::Dynamic,
            bound_issuer: Some(issuer.to_string()),
        };
        let store = MemoryOAuthClientRegistrationStore::new();
        store.save(issuer, &registration).await.unwrap();
        let options = OAuthClientRegistrationOptions::new().with_dynamic_registration(
            OAuthDynamicClientRegistration::native("test-client", ["http://127.0.0.1/callback"]),
        );
        let metadata = authorization_server_metadata(issuer);

        let resolved = resolve_oauth_client_registration_with_store(
            &reqwest::Client::new(),
            &metadata,
            &options,
            &store,
        )
        .await
        .unwrap();

        assert_eq!(resolved, registration);
        assert_eq!(store.len().await, 1);
    }

    #[tokio::test]
    async fn issuer_migration_registers_new_credentials_without_reusing_old() {
        let old_issuer = "https://old-auth.example.com";
        let new_issuer = "https://new-auth.example.com";
        let old_registration = OAuthClientRegistration {
            client_id: "old-client".to_string(),
            client_secret: Some("old-secret".to_string()),
            method: OAuthClientRegistrationMethod::Dynamic,
            bound_issuer: Some(old_issuer.to_string()),
        };
        let store = MemoryOAuthClientRegistrationStore::new();
        store.save(old_issuer, &old_registration).await.unwrap();
        let (registration_endpoint, registration_task) =
            dynamic_registration_endpoint("new-client", "new-secret").await;
        let mut metadata = authorization_server_metadata(new_issuer);
        metadata.registration_endpoint = Some(registration_endpoint);
        let options = OAuthClientRegistrationOptions::new().with_dynamic_registration(
            OAuthDynamicClientRegistration::native("test-client", ["http://127.0.0.1/callback"]),
        );

        let resolved = resolve_oauth_client_registration_with_store(
            &reqwest::Client::new(),
            &metadata,
            &options,
            &store,
        )
        .await
        .unwrap();
        registration_task.await.unwrap();

        assert_eq!(resolved.client_id(), "new-client");
        assert_eq!(resolved.bound_issuer(), Some(new_issuer));
        assert_eq!(store.len().await, 2);
        assert_eq!(
            store.load(old_issuer).await.unwrap(),
            Some(old_registration)
        );
        assert_eq!(
            store
                .load(new_issuer)
                .await
                .unwrap()
                .as_ref()
                .map(OAuthClientRegistration::client_id),
            Some("new-client")
        );
    }

    #[tokio::test]
    async fn corrupted_store_binding_is_rejected_instead_of_reused() {
        let issuer = "https://new-auth.example.com";
        let registration = OAuthClientRegistration {
            client_id: "old-client".to_string(),
            client_secret: None,
            method: OAuthClientRegistrationMethod::Dynamic,
            bound_issuer: Some("https://old-auth.example.com".to_string()),
        };
        let store = MemoryOAuthClientRegistrationStore::new();
        store.save(issuer, &registration).await.unwrap();
        let options = OAuthClientRegistrationOptions::new().with_dynamic_registration(
            OAuthDynamicClientRegistration::native("test-client", ["http://127.0.0.1/callback"]),
        );

        let error = resolve_oauth_client_registration_with_store(
            &reqwest::Client::new(),
            &authorization_server_metadata(issuer),
            &options,
            &store,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, OAuthClientError::CredentialStore(_)));
        assert!(error.to_string().contains("old-auth.example.com"));
    }

    #[tokio::test]
    async fn registration_reports_when_user_input_is_required() {
        let metadata = authorization_server_metadata("https://auth.example.com");
        let error = resolve_oauth_client_registration(
            &reqwest::Client::new(),
            &metadata,
            &OAuthClientRegistrationOptions::new(),
        )
        .await
        .unwrap_err();

        assert!(
            error.to_string().contains("prompt the user"),
            "got: {error}"
        );
    }

    fn authorization_server_metadata(issuer: &str) -> OAuthAuthorizationServerMetadata {
        OAuthAuthorizationServerMetadata {
            issuer: issuer.to_string(),
            authorization_endpoint: "https://auth.example.com/authorize".to_string(),
            token_endpoint: "https://auth.example.com/token".to_string(),
            registration_endpoint: None,
            client_id_metadata_document_supported: false,
            authorization_response_iss_parameter_supported: false,
            code_challenge_methods_supported: vec!["S256".to_string()],
            token_endpoint_auth_methods_supported: Vec::new(),
            grant_types_supported: Vec::new(),
            scopes_supported: Vec::new(),
        }
    }

    async fn dynamic_registration_endpoint(
        client_id: &'static str,
        client_secret: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0u8; 1024];
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&chunk[..read]);
                if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or_default();
            while bytes.len() < header_end + content_length {
                let mut chunk = [0u8; 1024];
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&chunk[..read]);
            }

            let body = serde_json::json!({
                "client_id": client_id,
                "client_secret": client_secret,
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 201 Created\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        (format!("http://{address}/register"), task)
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
