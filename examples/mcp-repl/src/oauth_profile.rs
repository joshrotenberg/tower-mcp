//! Secure, named OAuth credentials for mcp-repl.
//!
//! The TOML config contains only routing and registration metadata. Access
//! tokens, refresh tokens, and DCR client secrets are serialized into one
//! profile record in the operating-system credential store.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use toml_edit::{Array, DocumentMut, Item, Table, value};
use tower_mcp::client::{
    OAuthAuthorizationAction, OAuthAuthorizationFlow, OAuthAuthorizationHandler,
    OAuthAuthorizationRequest, OAuthClientError, OAuthClientRegistration,
    OAuthClientRegistrationOptions, OAuthClientRegistrationStore, OAuthDynamicClientRegistration,
    OAuthRedirectPolicy, OAuthStoredToken, OAuthTokenBinding, OAuthTokenStore,
};

use crate::config::OAuthProfile;

const KEYRING_SERVICE: &str = "mcp-repl/oauth";
// Credential Manager stores passwords as UTF-16 in a 2,560-byte blob. Base64
// of 768 raw bytes is 1,024 ASCII characters / 2,048 UTF-16 bytes, leaving
// room below that platform limit. The other supported stores allow more.
const KEYRING_CHUNK_BYTES: usize = 768;
const MAX_KEYRING_CHUNKS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyringManifest {
    generation: String,
    chunks: usize,
}

impl KeyringManifest {
    fn parse(encoded: &str) -> Result<Self, String> {
        let mut parts = encoded.split(':');
        let (Some("v1"), Some(generation), Some(chunks), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("stored OAuth credential manifest is invalid".to_string());
        };
        if generation.is_empty()
            || !generation
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            return Err("stored OAuth credential manifest has an invalid generation".to_string());
        }
        let chunks = chunks
            .parse::<usize>()
            .map_err(|_| "stored OAuth credential manifest has an invalid chunk count")?;
        if chunks == 0 || chunks > MAX_KEYRING_CHUNKS {
            return Err("stored OAuth credential manifest has an invalid chunk count".to_string());
        }
        Ok(Self {
            generation: generation.to_string(),
            chunks,
        })
    }

    fn encode(&self) -> String {
        format!("v1:{}:{}", self.generation, self.chunks)
    }
}

fn new_generation() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("cannot generate credential-store record id: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn chunk_account(profile: &str, manifest: &KeyringManifest, index: usize) -> String {
    format!("{profile}:{}:{index}", manifest.generation)
}

fn encode_chunks(secret: &str) -> Vec<String> {
    secret
        .as_bytes()
        .chunks(KEYRING_CHUNK_BYTES)
        .map(|chunk| base64::engine::general_purpose::STANDARD.encode(chunk))
        .collect()
}

fn decode_chunks(chunks: impl IntoIterator<Item = String>) -> Result<String, String> {
    let mut decoded = Vec::new();
    for chunk in chunks {
        decoded.extend(
            base64::engine::general_purpose::STANDARD
                .decode(chunk)
                .map_err(|_| "stored OAuth credential chunk is invalid")?,
        );
    }
    String::from_utf8(decoded)
        .map_err(|_| "stored OAuth credential record is not UTF-8".to_string())
}

fn keyring_entry(account: &str) -> Result<keyring::v1::Entry, String> {
    keyring::v1::Entry::new(KEYRING_SERVICE, account).map_err(|error| error.to_string())
}

fn delete_keyring_entry(account: &str) -> Result<(), String> {
    match keyring_entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::v1::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn read_keyring_manifest(profile: &str) -> Result<Option<KeyringManifest>, String> {
    match keyring_entry(profile)?.get_password() {
        Ok(encoded) => KeyringManifest::parse(&encoded).map(Some),
        Err(keyring::v1::Error::NoEntry) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn delete_keyring_generation(profile: &str, manifest: &KeyringManifest) -> Result<(), String> {
    let mut first_error = None;
    for index in 0..manifest.chunks {
        if let Err(error) = delete_keyring_entry(&chunk_account(profile, manifest, index))
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[async_trait]
trait SecretBackend: Send + Sync {
    async fn load(&self, profile: &str) -> Result<Option<String>, String>;
    async fn save(&self, profile: &str, secret: &str) -> Result<(), String>;
    async fn remove(&self, profile: &str) -> Result<(), String>;
}

#[derive(Debug, Default)]
struct KeyringSecretBackend;

impl KeyringSecretBackend {
    fn check() -> Result<(), String> {
        keyring::v1::Entry::store_status()
            .as_ref()
            .map_err(|error| {
                format!(
                    "the operating-system credential store is unavailable: {error}. \
                 mcp-repl will not fall back to plaintext; use MCP_BEARER for a headless \
                 environment or configure the platform credential service"
                )
            })
            .copied()
    }
}

#[async_trait]
impl SecretBackend for KeyringSecretBackend {
    async fn load(&self, profile: &str) -> Result<Option<String>, String> {
        let profile = profile.to_string();
        tokio::task::spawn_blocking(move || {
            let Some(manifest) = read_keyring_manifest(&profile)? else {
                return Ok(None);
            };
            let mut chunks = Vec::with_capacity(manifest.chunks);
            for index in 0..manifest.chunks {
                let account = chunk_account(&profile, &manifest, index);
                let chunk = keyring_entry(&account)?.get_password().map_err(|error| {
                    if matches!(error, keyring::v1::Error::NoEntry) {
                        "stored OAuth credential record is incomplete".to_string()
                    } else {
                        error.to_string()
                    }
                })?;
                chunks.push(chunk);
            }
            decode_chunks(chunks).map(Some)
        })
        .await
        .map_err(|error| format!("credential-store worker failed: {error}"))?
    }

    async fn save(&self, profile: &str, secret: &str) -> Result<(), String> {
        let profile = profile.to_string();
        let secret = secret.to_string();
        tokio::task::spawn_blocking(move || {
            let previous = read_keyring_manifest(&profile)?;
            let encoded_chunks = encode_chunks(&secret);
            if encoded_chunks.is_empty() || encoded_chunks.len() > MAX_KEYRING_CHUNKS {
                return Err("OAuth credential record is too large for the secure store".to_string());
            }
            let manifest = KeyringManifest {
                generation: new_generation()?,
                chunks: encoded_chunks.len(),
            };
            for (index, chunk) in encoded_chunks.iter().enumerate() {
                let account = chunk_account(&profile, &manifest, index);
                if let Err(error) = keyring_entry(&account)
                    .and_then(|entry| entry.set_password(chunk).map_err(|error| error.to_string()))
                {
                    for cleanup in 0..index {
                        let _ = delete_keyring_entry(&chunk_account(&profile, &manifest, cleanup));
                    }
                    return Err(error);
                }
            }
            if let Err(error) = keyring_entry(&profile).and_then(|entry| {
                entry
                    .set_password(&manifest.encode())
                    .map_err(|error| error.to_string())
            }) {
                let _ = delete_keyring_generation(&profile, &manifest);
                return Err(error);
            }
            if let Some(previous) = previous {
                delete_keyring_generation(&profile, &previous)?;
            }
            Ok(())
        })
        .await
        .map_err(|error| format!("credential-store worker failed: {error}"))?
    }

    async fn remove(&self, profile: &str) -> Result<(), String> {
        let profile = profile.to_string();
        tokio::task::spawn_blocking(move || {
            if let Some(manifest) = read_keyring_manifest(&profile)? {
                delete_keyring_generation(&profile, &manifest)?;
            }
            delete_keyring_entry(&profile)
        })
        .await
        .map_err(|error| format!("credential-store worker failed: {error}"))?
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredToken {
    binding: OAuthTokenBinding,
    token: OAuthStoredToken,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredRegistration {
    issuer: String,
    registration: OAuthClientRegistration,
}

#[derive(Default, Serialize, Deserialize)]
struct Secrets {
    #[serde(default)]
    tokens: Vec<StoredToken>,
    #[serde(default)]
    registrations: Vec<StoredRegistration>,
}

/// Both persistence traits share one encrypted/keychain record and one lock,
/// so token refresh and DCR updates cannot overwrite one another in-process.
#[derive(Clone)]
pub struct CredentialStore {
    profile: Arc<str>,
    backend: Arc<dyn SecretBackend>,
    lock: Arc<Mutex<()>>,
}

impl std::fmt::Debug for CredentialStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialStore")
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

impl CredentialStore {
    pub fn keyring(profile: &str) -> Result<Self, String> {
        validate_name(profile)?;
        KeyringSecretBackend::check()?;
        Ok(Self::with_backend(profile, Arc::new(KeyringSecretBackend)))
    }

    fn with_backend(profile: &str, backend: Arc<dyn SecretBackend>) -> Self {
        Self {
            profile: Arc::from(profile),
            backend,
            lock: Arc::new(Mutex::new(())),
        }
    }

    async fn load_secrets(&self) -> Result<Secrets, String> {
        let Some(encoded) = self.backend.load(&self.profile).await? else {
            return Ok(Secrets::default());
        };
        serde_json::from_str(&encoded)
            .map_err(|error| format!("stored OAuth profile is invalid: {error}"))
    }

    async fn save_secrets(&self, secrets: &Secrets) -> Result<(), String> {
        if secrets.tokens.is_empty() && secrets.registrations.is_empty() {
            return self.backend.remove(&self.profile).await;
        }
        let encoded = serde_json::to_string(secrets)
            .map_err(|error| format!("cannot encode OAuth credentials: {error}"))?;
        self.backend.save(&self.profile, &encoded).await
    }

    pub async fn clear(&self) -> Result<(), String> {
        let _guard = self.lock.lock().await;
        self.backend.remove(&self.profile).await
    }

    pub async fn clear_tokens(&self) -> Result<(), String> {
        let _guard = self.lock.lock().await;
        let mut secrets = self.load_secrets().await?;
        secrets.tokens.clear();
        self.save_secrets(&secrets).await
    }

    pub async fn has_tokens(&self) -> Result<bool, String> {
        let _guard = self.lock.lock().await;
        self.load_secrets()
            .await
            .map(|secrets| !secrets.tokens.is_empty())
    }
}

#[async_trait]
impl OAuthTokenStore for CredentialStore {
    async fn load(
        &self,
        binding: &OAuthTokenBinding,
    ) -> Result<Option<OAuthStoredToken>, OAuthClientError> {
        let _guard = self.lock.lock().await;
        self.load_secrets()
            .await
            .map(|secrets| {
                secrets
                    .tokens
                    .into_iter()
                    .find(|stored| stored.binding == *binding)
                    .map(|stored| stored.token)
            })
            .map_err(OAuthClientError::TokenStore)
    }

    async fn save(
        &self,
        binding: &OAuthTokenBinding,
        token: &OAuthStoredToken,
    ) -> Result<(), OAuthClientError> {
        let _guard = self.lock.lock().await;
        let mut secrets = self
            .load_secrets()
            .await
            .map_err(OAuthClientError::TokenStore)?;
        secrets.tokens.retain(|stored| stored.binding != *binding);
        secrets.tokens.push(StoredToken {
            binding: binding.clone(),
            token: token.clone(),
        });
        self.save_secrets(&secrets)
            .await
            .map_err(OAuthClientError::TokenStore)
    }

    async fn remove(&self, binding: &OAuthTokenBinding) -> Result<(), OAuthClientError> {
        let _guard = self.lock.lock().await;
        let mut secrets = self
            .load_secrets()
            .await
            .map_err(OAuthClientError::TokenStore)?;
        secrets.tokens.retain(|stored| stored.binding != *binding);
        self.save_secrets(&secrets)
            .await
            .map_err(OAuthClientError::TokenStore)
    }
}

#[async_trait]
impl OAuthClientRegistrationStore for CredentialStore {
    async fn load(
        &self,
        issuer: &str,
    ) -> Result<Option<OAuthClientRegistration>, OAuthClientError> {
        let _guard = self.lock.lock().await;
        self.load_secrets()
            .await
            .map(|secrets| {
                secrets
                    .registrations
                    .into_iter()
                    .find(|stored| stored.issuer == issuer)
                    .map(|stored| stored.registration)
            })
            .map_err(OAuthClientError::CredentialStore)
    }

    async fn save(
        &self,
        issuer: &str,
        registration: &OAuthClientRegistration,
    ) -> Result<(), OAuthClientError> {
        let _guard = self.lock.lock().await;
        let mut secrets = self
            .load_secrets()
            .await
            .map_err(OAuthClientError::CredentialStore)?;
        secrets
            .registrations
            .retain(|stored| stored.issuer != issuer);
        secrets.registrations.push(StoredRegistration {
            issuer: issuer.to_string(),
            registration: registration.clone(),
        });
        self.save_secrets(&secrets)
            .await
            .map_err(OAuthClientError::CredentialStore)
    }

    async fn remove(&self, issuer: &str) -> Result<(), OAuthClientError> {
        let _guard = self.lock.lock().await;
        let mut secrets = self
            .load_secrets()
            .await
            .map_err(OAuthClientError::CredentialStore)?;
        secrets
            .registrations
            .retain(|stored| stored.issuer != issuer);
        self.save_secrets(&secrets)
            .await
            .map_err(OAuthClientError::CredentialStore)
    }
}

type BrowserOpener = dyn Fn(&str) -> Result<(), String> + Send + Sync;

#[derive(Clone)]
struct BrowserAuthorizationHandler {
    profile: Arc<str>,
    interactive: bool,
    open_browser: bool,
    opener: Arc<BrowserOpener>,
}

impl BrowserAuthorizationHandler {
    fn new(profile: &str, interactive: bool, open_browser: bool) -> Self {
        Self {
            profile: Arc::from(profile),
            interactive,
            open_browser,
            opener: Arc::new(|url| webbrowser::open(url).map_err(|error| error.to_string())),
        }
    }
}

#[async_trait]
impl OAuthAuthorizationHandler for BrowserAuthorizationHandler {
    async fn authorize(
        &self,
        request: OAuthAuthorizationRequest,
    ) -> Result<OAuthAuthorizationAction, OAuthClientError> {
        if !self.interactive {
            return Err(OAuthClientError::Redirect(format!(
                "interactive authorization is required; run `mcp-repl --login {} --http {}` first",
                self.profile, request.resource
            )));
        }
        if self.open_browser {
            match (self.opener)(&request.authorization_url) {
                Ok(()) => eprintln!("OAuth authorization opened in your browser."),
                Err(error) => eprintln!("Could not open a browser ({error})."),
            }
        }
        eprintln!(
            "Authorize this client, then return here (waiting up to 5 minutes):\n{}",
            request.authorization_url
        );
        Ok(OAuthAuthorizationAction::AwaitLoopback)
    }
}

pub fn build_flow(
    name: &str,
    resource_url: &str,
    metadata: &OAuthProfile,
    interactive: bool,
    open_browser: bool,
) -> Result<(OAuthAuthorizationFlow, CredentialStore), String> {
    let store = CredentialStore::keyring(name)?;
    let mut options = OAuthClientRegistrationOptions::new().with_dynamic_registration(
        OAuthDynamicClientRegistration::native("mcp-repl", std::iter::empty::<String>()),
    );
    if let Some(client_id) = &metadata.client_id_metadata_document {
        options = options.with_client_id_metadata_document(client_id.clone());
    }
    let mut builder = OAuthAuthorizationFlow::builder(resource_url)
        .redirect_policy(OAuthRedirectPolicy::loopback())
        .registration_options(options)
        .registration_store(store.clone())
        .token_store(store.clone())
        .authorization_handler(BrowserAuthorizationHandler::new(
            name,
            interactive,
            open_browser,
        ));
    if let Some(issuer) = &metadata.authorization_server {
        builder = builder.preferred_authorization_server(issuer.clone());
    }
    let flow = builder.build().map_err(|error| error.to_string())?;
    Ok((flow, store))
}

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        return Err(
            "OAuth profile names must be 1-64 characters containing only ASCII letters, digits, \
             `.`, `_`, or `-`"
                .to_string(),
        );
    }
    Ok(())
}

pub fn save_metadata(path: &Path, name: &str, profile: &OAuthProfile) -> Result<(), String> {
    validate_name(name)?;
    edit_config(path, |document| {
        if document.get("oauth").is_none() {
            document["oauth"] = Item::Table(Table::new());
        }
        let oauth = document["oauth"]
            .as_table_mut()
            .ok_or("top-level `oauth` must be a table")?;
        let mut table = Table::new();
        table["url"] = value(&profile.url);
        if !profile.scopes.is_empty() {
            let mut scopes = Array::new();
            for scope in &profile.scopes {
                scopes.push(scope.as_str());
            }
            table["scopes"] = value(scopes);
        }
        if let Some(client_id) = &profile.client_id_metadata_document {
            table["client_id_metadata_document"] = value(client_id);
        }
        if let Some(issuer) = &profile.authorization_server {
            table["authorization_server"] = value(issuer);
        }
        oauth[name] = Item::Table(table);
        Ok(())
    })
}

pub fn remove_metadata(path: &Path, name: &str) -> Result<bool, String> {
    validate_name(name)?;
    let mut removed = false;
    edit_config(path, |document| {
        let Some(oauth) = document.get_mut("oauth") else {
            return Ok(());
        };
        let table = oauth
            .as_table_mut()
            .ok_or("top-level `oauth` must be a table")?;
        removed = table.remove(name).is_some();
        if table.is_empty() {
            document.remove("oauth");
        }
        Ok(())
    })?;
    Ok(removed)
}

fn edit_config(
    path: &Path,
    edit: impl FnOnce(&mut DocumentMut) -> Result<(), String>,
) -> Result<(), String> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let mut document = source
        .parse::<DocumentMut>()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    edit(&mut document)?;
    write_atomic(path, &document.to_string())
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn write_atomic(path: &Path, contents: &str) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, contents)?;
    std::fs::rename(temporary, path)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::RwLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Default)]
    struct MemoryBackend(RwLock<HashMap<String, String>>);

    #[async_trait]
    impl SecretBackend for MemoryBackend {
        async fn load(&self, profile: &str) -> Result<Option<String>, String> {
            Ok(self.0.read().unwrap().get(profile).cloned())
        }

        async fn save(&self, profile: &str, secret: &str) -> Result<(), String> {
            self.0
                .write()
                .unwrap()
                .insert(profile.to_string(), secret.to_string());
            Ok(())
        }

        async fn remove(&self, profile: &str) -> Result<(), String> {
            self.0.write().unwrap().remove(profile);
            Ok(())
        }
    }

    fn binding(resource: &str) -> OAuthTokenBinding {
        OAuthTokenBinding {
            resource: resource.to_string(),
            issuer: "https://auth.example".to_string(),
            client_id: "client".to_string(),
        }
    }

    #[test]
    fn secure_store_chunks_large_unicode_records_below_platform_limit() {
        let secret = format!("{}{}", "jwt.".repeat(1200), "🔐".repeat(700));
        let chunks = encode_chunks(&secret);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 1024));
        assert_eq!(decode_chunks(chunks).unwrap(), secret);

        let manifest = KeyringManifest {
            generation: "01abcdef".to_string(),
            chunks: 7,
        };
        assert_eq!(
            KeyringManifest::parse(&manifest.encode()).unwrap(),
            manifest
        );
        assert!(KeyringManifest::parse("v1:bad:not-a-number").is_err());
    }

    #[test]
    fn profile_names_are_bounded_for_platform_stores() {
        assert!(validate_name("work-prod_1.example").is_ok());
        assert!(validate_name(&"a".repeat(65)).is_err());
        assert!(validate_name("contains/slash").is_err());
    }

    fn token(access_token: &str) -> OAuthStoredToken {
        OAuthStoredToken {
            access_token: access_token.to_string(),
            refresh_token: Some("refresh-secret".to_string()),
            expires_at: u64::MAX,
            scopes: vec!["openid".to_string()],
        }
    }

    #[tokio::test]
    async fn exact_bindings_and_issuers_share_one_secret_record() {
        let backend = Arc::new(MemoryBackend::default());
        let store = CredentialStore::with_backend("work", backend.clone());
        OAuthTokenStore::save(&store, &binding("https://one/mcp"), &token("one"))
            .await
            .unwrap();
        OAuthTokenStore::save(&store, &binding("https://two/mcp"), &token("two"))
            .await
            .unwrap();
        let registration = OAuthClientRegistration::dynamically_registered(
            "https://auth.example",
            "client",
            Some("client-secret".to_string()),
        );
        OAuthClientRegistrationStore::save(&store, "https://auth.example", &registration)
            .await
            .unwrap();

        assert_eq!(
            OAuthTokenStore::load(&store, &binding("https://two/mcp"))
                .await
                .unwrap()
                .unwrap()
                .access_token,
            "two"
        );
        assert!(
            OAuthTokenStore::load(&store, &binding("https://other/mcp"))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            OAuthClientRegistrationStore::load(&store, "https://auth.example")
                .await
                .unwrap()
                .unwrap(),
            registration
        );

        let encoded = backend.0.read().unwrap()["work"].clone();
        assert!(encoded.contains("refresh-secret"));
        assert!(encoded.contains("client-secret"));
        assert!(!format!("{store:?}").contains("secret"));
    }

    #[tokio::test]
    async fn clear_removes_every_secret_for_profile() {
        let backend = Arc::new(MemoryBackend::default());
        let store = CredentialStore::with_backend("work", backend.clone());
        OAuthTokenStore::save(&store, &binding("https://one/mcp"), &token("one"))
            .await
            .unwrap();
        store.clear().await.unwrap();
        assert!(backend.0.read().unwrap().is_empty());
    }

    #[tokio::test]
    async fn clearing_tokens_preserves_dynamic_registration() {
        let backend = Arc::new(MemoryBackend::default());
        let store = CredentialStore::with_backend("work", backend);
        OAuthTokenStore::save(&store, &binding("https://one/mcp"), &token("one"))
            .await
            .unwrap();
        let registration = OAuthClientRegistration::dynamically_registered(
            "https://auth.example",
            "client",
            Some("client-secret".to_string()),
        );
        OAuthClientRegistrationStore::save(&store, "https://auth.example", &registration)
            .await
            .unwrap();

        assert!(store.has_tokens().await.unwrap());
        store.clear_tokens().await.unwrap();
        assert!(!store.has_tokens().await.unwrap());
        assert!(
            OAuthTokenStore::load(&store, &binding("https://one/mcp"))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            OAuthClientRegistrationStore::load(&store, "https://auth.example")
                .await
                .unwrap(),
            Some(registration)
        );
    }

    fn request() -> OAuthAuthorizationRequest {
        OAuthAuthorizationRequest {
            authorization_url: "https://auth.example/authorize?state=test".to_string(),
            redirect_uri: "http://127.0.0.1:12345/callback".to_string(),
            resource: "https://mcp.example/mcp".to_string(),
            issuer: "https://auth.example".to_string(),
            scopes: vec!["openid".to_string()],
        }
    }

    #[tokio::test]
    async fn browser_handler_has_an_automation_safe_seam() {
        let opens = Arc::new(AtomicUsize::new(0));
        let opener = {
            let opens = opens.clone();
            Arc::new(move |_url: &str| {
                opens.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }) as Arc<BrowserOpener>
        };
        let headless = BrowserAuthorizationHandler {
            profile: Arc::from("work"),
            interactive: false,
            open_browser: true,
            opener: opener.clone(),
        };
        let error = headless.authorize(request()).await.unwrap_err();
        assert!(error.to_string().contains("--login work"));
        assert_eq!(opens.load(Ordering::SeqCst), 0);

        let manual = BrowserAuthorizationHandler {
            profile: Arc::from("work"),
            interactive: true,
            open_browser: false,
            opener: opener.clone(),
        };
        assert!(matches!(
            manual.authorize(request()).await.unwrap(),
            OAuthAuthorizationAction::AwaitLoopback
        ));
        assert_eq!(opens.load(Ordering::SeqCst), 0);

        let browser = BrowserAuthorizationHandler {
            profile: Arc::from("work"),
            interactive: true,
            open_browser: true,
            opener,
        };
        assert!(matches!(
            browser.authorize(request()).await.unwrap(),
            OAuthAuthorizationAction::AwaitLoopback
        ));
        assert_eq!(opens.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn metadata_round_trip_never_contains_credentials() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");
        std::fs::write(&path, "[aliases]\nt = \"tools\"\n").unwrap();
        let profile = OAuthProfile {
            url: "https://mcp.example/mcp".to_string(),
            scopes: vec!["openid".to_string(), "offline_access".to_string()],
            client_id_metadata_document: Some("https://client.example/metadata.json".to_string()),
            authorization_server: Some("https://auth.example".to_string()),
        };

        save_metadata(&path, "work", &profile).unwrap();
        let source = std::fs::read_to_string(&path).unwrap();
        assert!(source.contains("[oauth.work]"));
        assert!(source.contains("[aliases]"));
        assert!(!source.contains("token"));
        assert!(!source.contains("secret"));
        assert_eq!(
            crate::config::Config::parse(&source).unwrap().oauth["work"],
            profile
        );

        assert!(remove_metadata(&path, "work").unwrap());
        let source = std::fs::read_to_string(path).unwrap();
        assert!(!source.contains("[oauth"));
        assert!(source.contains("[aliases]"));
    }
}
