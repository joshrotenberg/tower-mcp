//! Typed server support for the stable MCP Apps extension (SEP-1865).
//!
//! This module deliberately separates compile-time availability from runtime
//! activation:
//!
//! - enable the `mcp-apps` Cargo feature to compile these APIs;
//! - call [`McpRouter::with_mcp_apps`] or [`McpClientBuilder::with_mcp_apps`]
//!   to declare support on the wire;
//! - check [`RequestContext::supports_mcp_apps`] before applying
//!   extension-specific behavior.
//!
//! UI resources built here use the required `ui://` scheme and
//! `text/html;profile=mcp-app` MIME type. External CSP sources are restricted
//! to origins: credentials, paths, queries, fragments, and unsupported schemes
//! are rejected.
//!
//! See [`crate::guides::mcp_apps`] for the task-oriented setup, security
//! boundary, CSP and permissions policy, and visibility guidance.

use std::collections::HashSet;
use std::fmt;

use serde::Serialize;
use serde_json::{Map, Value};
use thiserror::Error;
use url::Url;

use crate::protocol::{
    CallToolResult, Content, MetaValidationError, ReadResourceResult, ResourceContent,
    validate_meta_object,
};
use crate::{
    ExtensionDeclaration, McpClientBuilder, McpRouter, RequestContext, Resource, ResourceBuilder,
    Tool,
};

/// Stable extension identifier reserved for MCP Apps.
pub const MCP_APPS_EXTENSION_ID: &str = "io.modelcontextprotocol/ui";

/// The only content type defined by the stable MCP Apps MVP.
pub const MCP_APP_HTML_MIME_TYPE: &str = "text/html;profile=mcp-app";

/// A validation or construction error from the typed MCP Apps API.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum McpAppError {
    /// The resource URI did not satisfy the MCP Apps `ui://` requirement.
    #[error("invalid MCP Apps UI URI {0:?}")]
    InvalidUiUri(String),
    /// The resource body did not look like a complete HTML5 document.
    #[error("MCP Apps content must be a complete HTML5 document")]
    InvalidHtmlDocument,
    /// A CSP source was not a permitted origin.
    #[error("invalid MCP Apps CSP origin {origin:?} for {directive}")]
    InvalidCspOrigin {
        /// The rejected source.
        origin: String,
        /// The metadata field being configured.
        directive: &'static str,
    },
    /// A dedicated sandbox domain was malformed.
    #[error("invalid MCP Apps dedicated domain {0:?}")]
    InvalidDomain(String),
    /// Tool visibility was empty or contained duplicate entries.
    #[error("MCP Apps tool visibility must be non-empty and contain no duplicates")]
    InvalidVisibility,
    /// A required human-readable value was empty.
    #[error("MCP Apps {0} must not be empty")]
    EmptyField(&'static str),
    /// Protocol metadata failed the core `_meta` grammar.
    #[error(transparent)]
    Metadata(#[from] MetaValidationError),
    /// Typed metadata could not be converted to JSON.
    #[error("failed to serialize MCP Apps metadata: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// A validated MCP Apps resource identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct McpAppUri(String);

impl McpAppUri {
    /// Validate an MCP Apps resource URI.
    pub fn new(uri: impl Into<String>) -> Result<Self, McpAppError> {
        let uri = uri.into();
        let parsed = Url::parse(&uri).map_err(|_| McpAppError::InvalidUiUri(uri.clone()))?;
        if !uri.starts_with("ui://")
            || parsed.scheme() != "ui"
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.port().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(McpAppError::InvalidUiUri(uri));
        }
        Ok(Self(uri))
    }

    /// Borrow the wire URI.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for McpAppUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl TryFrom<String> for McpAppUri {
    type Error = McpAppError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for McpAppUri {
    type Error = McpAppError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// A minimally validated complete HTML5 document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpAppHtml(String);

impl McpAppHtml {
    /// Validate a complete HTML5 document.
    ///
    /// This structural check requires an HTML5 doctype, an `<html` root, and
    /// rejects NUL bytes. It is intentionally not a sanitizer: MCP Apps hosts
    /// must still sandbox the document and enforce its declared CSP.
    pub fn new(html: impl Into<String>) -> Result<Self, McpAppError> {
        let html = html.into();
        let lower = html.trim_start().to_ascii_lowercase();
        if html.contains('\0') || !lower.starts_with("<!doctype html>") || !lower.contains("<html")
        {
            return Err(McpAppError::InvalidHtmlDocument);
        }
        Ok(Self(html))
    }

    /// Borrow the HTML source.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
struct CspOrigin(String);

#[derive(Debug, Clone, Copy)]
enum CspDirective {
    Connect,
    Resource,
    Frame,
    BaseUri,
}

impl CspDirective {
    fn name(self) -> &'static str {
        match self {
            Self::Connect => "connectDomains",
            Self::Resource => "resourceDomains",
            Self::Frame => "frameDomains",
            Self::BaseUri => "baseUriDomains",
        }
    }

    fn allows_scheme(self, scheme: &str) -> bool {
        match self {
            Self::Connect => matches!(scheme, "http" | "https" | "ws" | "wss"),
            Self::Resource | Self::Frame | Self::BaseUri => matches!(scheme, "http" | "https"),
        }
    }

    fn allows_wildcard(self) -> bool {
        matches!(self, Self::Resource)
    }
}

impl CspOrigin {
    fn parse(origin: impl Into<String>, directive: CspDirective) -> Result<Self, McpAppError> {
        let origin = origin.into();
        let invalid = || McpAppError::InvalidCspOrigin {
            origin: origin.clone(),
            directive: directive.name(),
        };
        if origin.trim() != origin || origin.ends_with("//") {
            return Err(invalid());
        }

        let wildcard = origin.contains("://*.");
        if wildcard && !directive.allows_wildcard() {
            return Err(invalid());
        }
        if origin.contains('*') && !wildcard {
            return Err(invalid());
        }

        let parseable = if wildcard {
            origin.replacen("://*.", "://wildcard.", 1)
        } else {
            origin.clone()
        };
        let parsed = Url::parse(&parseable).map_err(|_| invalid())?;
        if !directive.allows_scheme(parsed.scheme())
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || !matches!(parsed.path(), "" | "/")
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(invalid());
        }
        if wildcard {
            let host = parsed.host_str().ok_or_else(invalid)?;
            let suffix = host.strip_prefix("wildcard.").ok_or_else(invalid)?;
            if !suffix.contains('.') {
                return Err(invalid());
            }
        }

        Ok(Self(
            origin.strip_suffix('/').unwrap_or(&origin).to_string(),
        ))
    }
}

/// Content Security Policy inputs declared by an MCP Apps resource.
///
/// Empty fields are omitted, giving hosts the specification's restrictive
/// default. No API in this type emits wildcard `*`, `data:`, `'unsafe-eval'`,
/// or arbitrary CSP fragments.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpUiResourceCsp {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    connect_domains: Vec<CspOrigin>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    resource_domains: Vec<CspOrigin>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    frame_domains: Vec<CspOrigin>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    base_uri_domains: Vec<CspOrigin>,
}

impl McpUiResourceCsp {
    /// Allow one HTTP(S) or WebSocket origin for fetch/XHR/WebSocket.
    pub fn allow_connect(mut self, origin: impl Into<String>) -> Result<Self, McpAppError> {
        push_unique(
            &mut self.connect_domains,
            CspOrigin::parse(origin, CspDirective::Connect)?,
        );
        Ok(self)
    }

    /// Allow one HTTP(S) origin for scripts, styles, images, fonts, or media.
    ///
    /// This is the only field that accepts a wildcard subdomain such as
    /// `https://*.example.com`, matching SEP-1865.
    pub fn allow_resource(mut self, origin: impl Into<String>) -> Result<Self, McpAppError> {
        push_unique(
            &mut self.resource_domains,
            CspOrigin::parse(origin, CspDirective::Resource)?,
        );
        Ok(self)
    }

    /// Allow one HTTP(S) origin for nested iframes.
    pub fn allow_frame(mut self, origin: impl Into<String>) -> Result<Self, McpAppError> {
        push_unique(
            &mut self.frame_domains,
            CspOrigin::parse(origin, CspDirective::Frame)?,
        );
        Ok(self)
    }

    /// Allow one HTTP(S) origin for the document's base URI.
    pub fn allow_base_uri(mut self, origin: impl Into<String>) -> Result<Self, McpAppError> {
        push_unique(
            &mut self.base_uri_domains,
            CspOrigin::parse(origin, CspDirective::BaseUri)?,
        );
        Ok(self)
    }

    /// Declared connection origins.
    pub fn connect_domains(&self) -> impl Iterator<Item = &str> {
        self.connect_domains.iter().map(|origin| origin.0.as_str())
    }

    /// Declared static-resource origins.
    pub fn resource_domains(&self) -> impl Iterator<Item = &str> {
        self.resource_domains.iter().map(|origin| origin.0.as_str())
    }

    /// Declared nested-frame origins.
    pub fn frame_domains(&self) -> impl Iterator<Item = &str> {
        self.frame_domains.iter().map(|origin| origin.0.as_str())
    }

    /// Declared base-URI origins.
    pub fn base_uri_domains(&self) -> impl Iterator<Item = &str> {
        self.base_uri_domains.iter().map(|origin| origin.0.as_str())
    }
}

fn push_unique(values: &mut Vec<CspOrigin>, value: CspOrigin) {
    if !values.contains(&value) {
        values.push(value);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
struct EmptyPermission {}

/// Browser permissions requested by an MCP App.
///
/// Hosts may deny every requested permission; Apps must use feature detection
/// and remain functional when permissions are unavailable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpUiPermissions {
    #[serde(skip_serializing_if = "Option::is_none")]
    camera: Option<EmptyPermission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    microphone: Option<EmptyPermission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    geolocation: Option<EmptyPermission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    clipboard_write: Option<EmptyPermission>,
}

impl McpUiPermissions {
    /// Request camera access.
    pub fn camera(mut self) -> Self {
        self.camera = Some(EmptyPermission {});
        self
    }

    /// Request microphone access.
    pub fn microphone(mut self) -> Self {
        self.microphone = Some(EmptyPermission {});
        self
    }

    /// Request geolocation access.
    pub fn geolocation(mut self) -> Self {
        self.geolocation = Some(EmptyPermission {});
        self
    }

    /// Request clipboard-write access.
    pub fn clipboard_write(mut self) -> Self {
        self.clipboard_write = Some(EmptyPermission {});
        self
    }
}

/// A validated host-dependent dedicated sandbox domain.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct McpAppDomain(String);

impl McpAppDomain {
    /// Validate a bare DNS-style domain without a scheme, port, or path.
    pub fn new(domain: impl Into<String>) -> Result<Self, McpAppError> {
        let domain = domain.into();
        let valid = !domain.is_empty()
            && domain.len() <= 253
            && domain.trim() == domain
            && !domain.contains(['/', ':', '?', '#', '@'])
            && domain.split('.').all(|label| {
                let bytes = label.as_bytes();
                !bytes.is_empty()
                    && bytes.len() <= 63
                    && bytes[0].is_ascii_alphanumeric()
                    && bytes[bytes.len() - 1].is_ascii_alphanumeric()
                    && bytes
                        .iter()
                        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
            });
        if !valid {
            return Err(McpAppError::InvalidDomain(domain));
        }
        Ok(Self(domain))
    }

    /// Borrow the dedicated domain.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Rendering and security metadata for a UI resource.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpUiResourceMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    csp: Option<McpUiResourceCsp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    permissions: Option<McpUiPermissions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    domain: Option<McpAppDomain>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prefers_border: Option<bool>,
}

impl McpUiResourceMeta {
    /// Declare the external origins the App needs.
    pub fn csp(mut self, csp: McpUiResourceCsp) -> Self {
        self.csp = Some(csp);
        self
    }

    /// Declare optional browser permissions.
    pub fn permissions(mut self, permissions: McpUiPermissions) -> Self {
        self.permissions = Some(permissions);
        self
    }

    /// Request a host-specific dedicated sandbox domain.
    pub fn domain(mut self, domain: McpAppDomain) -> Self {
        self.domain = Some(domain);
        self
    }

    /// Request whether the host displays a visible border/background.
    pub fn prefers_border(mut self, prefers_border: bool) -> Self {
        self.prefers_border = Some(prefers_border);
        self
    }
}

/// Who may see or call a UI-linked tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum McpUiToolVisibility {
    /// Visible and callable by the model/agent.
    Model,
    /// Callable by an App from the same server connection.
    App,
}

/// MCP Apps metadata attached to a tool definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpUiToolMeta {
    resource_uri: McpAppUri,
    #[serde(skip_serializing_if = "Option::is_none")]
    visibility: Option<Vec<McpUiToolVisibility>>,
}

impl McpUiToolMeta {
    /// Link a tool to a UI resource.
    ///
    /// Omitted visibility uses the specification default of model + App.
    pub fn new(resource_uri: McpAppUri) -> Self {
        Self {
            resource_uri,
            visibility: None,
        }
    }

    /// Make a tool callable only by Apps, not visible to the model.
    pub fn app_only(resource_uri: McpAppUri) -> Self {
        Self::new(resource_uri).visibility_unchecked(vec![McpUiToolVisibility::App])
    }

    /// Make a tool visible only to the model, not callable from Apps.
    pub fn model_only(resource_uri: McpAppUri) -> Self {
        Self::new(resource_uri).visibility_unchecked(vec![McpUiToolVisibility::Model])
    }

    /// Set an explicit non-empty, duplicate-free visibility policy.
    pub fn visibility(
        mut self,
        visibility: impl IntoIterator<Item = McpUiToolVisibility>,
    ) -> Result<Self, McpAppError> {
        let visibility: Vec<_> = visibility.into_iter().collect();
        let unique: HashSet<_> = visibility.iter().copied().collect();
        if visibility.is_empty() || unique.len() != visibility.len() {
            return Err(McpAppError::InvalidVisibility);
        }
        self.visibility = Some(visibility);
        Ok(self)
    }

    fn visibility_unchecked(mut self, visibility: Vec<McpUiToolVisibility>) -> Self {
        self.visibility = Some(visibility);
        self
    }

    /// Linked UI resource URI.
    pub fn resource_uri(&self) -> &McpAppUri {
        &self.resource_uri
    }

    /// Effective visibility, including the model + App default.
    pub fn effective_visibility(&self) -> impl Iterator<Item = McpUiToolVisibility> + '_ {
        self.visibility
            .as_deref()
            .unwrap_or(&[McpUiToolVisibility::Model, McpUiToolVisibility::App])
            .iter()
            .copied()
    }
}

/// Capability settings advertised for MCP Apps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpAppsCapabilitySettings {
    mime_types: Vec<String>,
}

impl McpAppsCapabilitySettings {
    /// Settings for the stable raw-HTML MCP Apps MVP.
    pub fn html() -> Self {
        Self {
            mime_types: vec![MCP_APP_HTML_MIME_TYPE.to_string()],
        }
    }

    /// Supported MIME types.
    pub fn mime_types(&self) -> impl Iterator<Item = &str> {
        self.mime_types.iter().map(String::as_str)
    }
}

/// Build the validated core extension declaration for MCP Apps.
pub fn mcp_apps_extension() -> ExtensionDeclaration {
    ExtensionDeclaration::new(MCP_APPS_EXTENSION_ID, McpAppsCapabilitySettings::html())
        .expect("the built-in MCP Apps extension declaration is valid")
}

impl McpRouter {
    /// Advertise stable MCP Apps HTML support from this server.
    pub fn with_mcp_apps(self) -> Self {
        self.with_protocol_extension(mcp_apps_extension())
    }
}

impl McpClientBuilder {
    /// Advertise stable MCP Apps HTML support from this client/host.
    pub fn with_mcp_apps(self) -> Self {
        self.with_protocol_extension(mcp_apps_extension())
    }
}

impl RequestContext {
    /// Return whether both peers negotiated MCP Apps with the stable HTML MIME.
    pub fn supports_mcp_apps(&self) -> bool {
        let Some(extension) = self
            .negotiated_extensions()
            .and_then(|extensions| extensions.get(MCP_APPS_EXTENSION_ID))
        else {
            return false;
        };
        settings_support_html(extension.client_settings())
            && settings_support_html(extension.server_settings())
    }
}

fn settings_support_html(settings: &Value) -> bool {
    settings
        .get("mimeTypes")
        .and_then(Value::as_array)
        .is_some_and(|mime_types| {
            mime_types
                .iter()
                .any(|mime_type| mime_type.as_str() == Some(MCP_APP_HTML_MIME_TYPE))
        })
}

impl Tool {
    /// Attach typed `_meta.ui` linkage and visibility to this built tool.
    ///
    /// Existing unrelated metadata keys are preserved. A prior `ui` key is
    /// replaced by the typed value.
    pub fn with_mcp_app(mut self, metadata: McpUiToolMeta) -> Result<Self, McpAppError> {
        let ui = serde_json::to_value(metadata)?;
        self.meta = Some(merge_ui_meta(self.meta.take(), ui)?);
        Ok(self)
    }
}

fn merge_ui_meta(existing: Option<Value>, ui: Value) -> Result<Value, McpAppError> {
    let mut object = match existing {
        Some(Value::Object(object)) => object,
        Some(_) => return Err(McpAppError::Metadata(MetaValidationError::ExpectedObject)),
        None => Map::new(),
    };
    object.insert("ui".to_string(), ui);
    let meta = Value::Object(object);
    validate_meta_object(&meta)?;
    Ok(meta)
}

/// Builder for one predeclared raw-HTML MCP Apps resource.
pub struct McpAppResourceBuilder {
    uri: McpAppUri,
    name: String,
    description: Option<String>,
    html: McpAppHtml,
    metadata: McpUiResourceMeta,
}

impl McpAppResourceBuilder {
    /// Create a UI resource with validated URI and HTML content.
    pub fn new(
        uri: impl Into<String>,
        name: impl Into<String>,
        html: impl Into<String>,
    ) -> Result<Self, McpAppError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(McpAppError::EmptyField("resource name"));
        }
        Ok(Self {
            uri: McpAppUri::new(uri)?,
            name,
            description: None,
            html: McpAppHtml::new(html)?,
            metadata: McpUiResourceMeta::default(),
        })
    }

    /// Set a human-readable resource description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set typed UI security/rendering metadata.
    pub fn metadata(mut self, metadata: McpUiResourceMeta) -> Self {
        self.metadata = metadata;
        self
    }

    /// Build a regular [`Resource`] with Apps metadata on both the declaration
    /// and returned content.
    pub fn build(self) -> Result<Resource, McpAppError> {
        let uri = self.uri.to_string();
        let html = self.html.0;
        let meta = merge_ui_meta(None, serde_json::to_value(self.metadata)?)?;
        let content_meta = meta.clone();
        let content_uri = uri.clone();

        let mut builder = ResourceBuilder::new(uri)
            .name(self.name)
            .mime_type(MCP_APP_HTML_MIME_TYPE);
        if let Some(description) = self.description {
            builder = builder.description(description);
        }
        let resource = builder
            .handler(move || {
                let uri = content_uri.clone();
                let html = html.clone();
                let meta = content_meta.clone();
                async move {
                    Ok(ReadResourceResult {
                        contents: vec![ResourceContent {
                            uri,
                            mime_type: Some(MCP_APP_HTML_MIME_TYPE.to_string()),
                            text: Some(html),
                            blob: None,
                            meta: Some(meta),
                        }],
                        ..ReadResourceResult::default()
                    })
                }
            })
            .build()
            .with_meta(meta)?;
        Ok(resource)
    }
}

/// Construct an Apps-friendly tool result with a useful text-only fallback.
///
/// The fallback remains meaningful for hosts that did not negotiate MCP Apps;
/// the structured content is available to a rendered App.
pub fn mcp_app_tool_result(
    fallback_text: impl Into<String>,
    structured_content: impl Serialize,
) -> Result<CallToolResult, serde_json::Error> {
    Ok(CallToolResult {
        content: vec![Content::text(fallback_text)],
        is_error: false,
        structured_content: Some(serde_json::to_value(structured_content)?),
        meta: None,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;
    use crate::protocol::{ClientCapabilities, ServerCapabilities};

    const HTML: &str = "<!doctype html><html><body>weather</body></html>";

    #[test]
    fn ui_uri_rejects_non_ui_and_ambiguous_values() {
        assert!(McpAppUri::new("ui://weather/dashboard").is_ok());
        for invalid in [
            "https://example.com/app",
            "ui:///missing-authority",
            "ui://user@example.com/app",
            "ui://example.com/app?mode=wide",
            "UI://example.com/app",
        ] {
            assert!(McpAppUri::new(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn html_requires_a_complete_document_shape() {
        assert!(McpAppHtml::new(HTML).is_ok());
        assert!(McpAppHtml::new("<div>fragment</div>").is_err());
        assert!(McpAppHtml::new("<!doctype html><body>missing root</body>").is_err());
    }

    #[test]
    fn csp_accepts_only_field_appropriate_origins() {
        let csp = McpUiResourceCsp::default()
            .allow_connect("wss://events.example.com")
            .unwrap()
            .allow_resource("https://*.cdn.example.com")
            .unwrap()
            .allow_frame("https://player.example.com")
            .unwrap()
            .allow_base_uri("https://assets.example.com/")
            .unwrap();

        assert_eq!(
            serde_json::to_value(csp).unwrap(),
            json!({
                "connectDomains": ["wss://events.example.com"],
                "resourceDomains": ["https://*.cdn.example.com"],
                "frameDomains": ["https://player.example.com"],
                "baseUriDomains": ["https://assets.example.com"]
            })
        );
        assert!(
            McpUiResourceCsp::default()
                .allow_connect("https://example.com/api")
                .is_err()
        );
        assert!(
            McpUiResourceCsp::default()
                .allow_frame("https://*.example.com")
                .is_err()
        );
        assert!(
            McpUiResourceCsp::default()
                .allow_resource("data:text/javascript,alert(1)")
                .is_err()
        );
    }

    #[test]
    fn permissions_serialize_as_empty_capability_objects() {
        assert_eq!(
            serde_json::to_value(
                McpUiPermissions::default()
                    .camera()
                    .geolocation()
                    .clipboard_write()
            )
            .unwrap(),
            json!({"camera": {}, "geolocation": {}, "clipboardWrite": {}})
        );
    }

    #[test]
    fn visibility_is_typed_and_validated() {
        let uri = McpAppUri::new("ui://weather/dashboard").unwrap();
        assert_eq!(
            McpUiToolMeta::new(uri.clone())
                .effective_visibility()
                .collect::<Vec<_>>(),
            vec![McpUiToolVisibility::Model, McpUiToolVisibility::App]
        );
        assert!(McpUiToolMeta::new(uri.clone()).visibility([]).is_err());
        assert!(
            McpUiToolMeta::new(uri)
                .visibility([McpUiToolVisibility::App, McpUiToolVisibility::App])
                .is_err()
        );
    }

    #[tokio::test]
    async fn app_resource_uses_exact_wire_shape_on_definition_and_content() {
        let metadata = McpUiResourceMeta::default()
            .csp(
                McpUiResourceCsp::default()
                    .allow_connect("https://api.example.com")
                    .unwrap(),
            )
            .permissions(McpUiPermissions::default().geolocation())
            .domain(McpAppDomain::new("app.example.com").unwrap())
            .prefers_border(true);
        let resource = McpAppResourceBuilder::new("ui://weather/dashboard", "Weather", HTML)
            .unwrap()
            .metadata(metadata)
            .build()
            .unwrap();

        let definition = serde_json::to_value(resource.definition()).unwrap();
        assert_eq!(definition["mimeType"], MCP_APP_HTML_MIME_TYPE);
        assert_eq!(definition["_meta"]["ui"]["prefersBorder"], true);
        assert_eq!(
            definition["_meta"]["ui"]["csp"]["connectDomains"][0],
            "https://api.example.com"
        );

        let result = resource.read().await;
        assert_eq!(result.contents[0].uri, "ui://weather/dashboard");
        assert_eq!(
            result.contents[0].mime_type.as_deref(),
            Some(MCP_APP_HTML_MIME_TYPE)
        );
        assert_eq!(
            result.contents[0].meta.as_ref().unwrap()["ui"]["permissions"]["geolocation"],
            json!({})
        );
    }

    #[test]
    fn tool_metadata_preserves_other_extension_keys() {
        let tool = crate::ToolBuilder::new("weather")
            .handler(|()| async { Ok(CallToolResult::text("sunny")) })
            .build()
            .with_meta(json!({"com.example/audit": {"level": "full"}}))
            .unwrap()
            .with_mcp_app(McpUiToolMeta::app_only(
                McpAppUri::new("ui://weather/dashboard").unwrap(),
            ))
            .unwrap();

        let definition = serde_json::to_value(tool.definition()).unwrap();
        assert_eq!(definition["_meta"]["com.example/audit"]["level"], "full");
        assert_eq!(
            definition["_meta"]["ui"]["resourceUri"],
            "ui://weather/dashboard"
        );
        assert_eq!(definition["_meta"]["ui"]["visibility"], json!(["app"]));
    }

    #[test]
    fn runtime_support_requires_both_peers_and_the_html_mime() {
        let client = ClientCapabilities {
            extensions: Some(HashMap::from([(
                MCP_APPS_EXTENSION_ID.to_string(),
                serde_json::to_value(McpAppsCapabilitySettings::html()).unwrap(),
            )])),
            ..ClientCapabilities::default()
        };
        let server = ServerCapabilities {
            extensions: Some(HashMap::from([(
                MCP_APPS_EXTENSION_ID.to_string(),
                serde_json::to_value(McpAppsCapabilitySettings::html()).unwrap(),
            )])),
            ..ServerCapabilities::default()
        };
        let mut context = RequestContext::new(crate::protocol::RequestId::Number(1));
        context
            .extensions_mut()
            .insert(crate::NegotiatedExtensions::from_capabilities(
                &client, &server,
            ));
        assert!(context.supports_mcp_apps());

        let mismatched_server = ServerCapabilities {
            extensions: Some(HashMap::from([(
                MCP_APPS_EXTENSION_ID.to_string(),
                json!({"mimeTypes": ["text/plain"]}),
            )])),
            ..ServerCapabilities::default()
        };
        context
            .extensions_mut()
            .insert(crate::NegotiatedExtensions::from_capabilities(
                &client,
                &mismatched_server,
            ));
        assert!(!context.supports_mcp_apps());
    }

    #[test]
    fn opt_in_uses_the_exact_reserved_declaration() {
        let extension = mcp_apps_extension();
        let encoded = serde_json::to_value(extension.settings()).unwrap();
        assert_eq!(encoded["mimeTypes"][0], MCP_APP_HTML_MIME_TYPE);
        assert_eq!(extension.identifier(), MCP_APPS_EXTENSION_ID);

        // Both methods remain explicit runtime opt-ins and compile independently
        // of the core protocol-version feature.
        let _router = McpRouter::new().with_mcp_apps();
        let _client = McpClientBuilder::new().with_mcp_apps();
    }

    #[test]
    fn result_helper_always_carries_text_fallback() {
        let result = mcp_app_tool_result("72 F and sunny", json!({"temperature": 72})).unwrap();
        assert_eq!(result.first_text(), Some("72 F and sunny"));
        assert_eq!(result.structured_content.unwrap()["temperature"], 72);
    }
}
