//! Protocol extension declarations and runtime negotiation.
//!
//! MCP extensions are opt-in. A client and server each declare extension
//! support under `capabilities.extensions`; an extension is active only when
//! both peers declare the same identifier. Unknown extensions remain
//! round-trippable protocol data but are never activated implicitly.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde_json::Value;

use crate::protocol::{
    ClientCapabilities, MetaValidationError, ServerCapabilities, validate_extension_identifier,
};

/// A validated local MCP protocol-extension declaration.
///
/// Extension identifiers use the mandatory vendor-prefix form defined by
/// SEP-2133, for example `io.modelcontextprotocol/ui`. Settings must serialize
/// to a JSON object; use [`empty`](Self::empty) when an extension has no
/// settings.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionDeclaration {
    identifier: String,
    settings: Value,
}

impl ExtensionDeclaration {
    /// Declare an extension with an empty settings object.
    pub fn empty(identifier: impl Into<String>) -> Result<Self, MetaValidationError> {
        Self::new(identifier, serde_json::json!({}))
    }

    /// Declare an extension with typed settings.
    pub fn new(
        identifier: impl Into<String>,
        settings: impl Serialize,
    ) -> Result<Self, MetaValidationError> {
        let identifier = identifier.into();
        validate_extension_identifier(&identifier)?;
        let settings = serde_json::to_value(settings)
            .map_err(|_| MetaValidationError::InvalidExtensionSettings(identifier.clone()))?;
        if !settings.is_object() {
            return Err(MetaValidationError::InvalidExtensionSettings(identifier));
        }
        Ok(Self {
            identifier,
            settings,
        })
    }

    /// Extension identifier.
    pub fn identifier(&self) -> &str {
        &self.identifier
    }

    /// Extension-defined local settings object.
    pub fn settings(&self) -> &Value {
        &self.settings
    }

    pub(crate) fn into_parts(self) -> (String, Value) {
        (self.identifier, self.settings)
    }
}

/// The settings each peer declared for one negotiated extension.
#[derive(Debug, Clone, PartialEq)]
pub struct NegotiatedExtension {
    client_settings: Value,
    server_settings: Value,
}

impl NegotiatedExtension {
    /// Settings advertised by the MCP client.
    pub fn client_settings(&self) -> &Value {
        &self.client_settings
    }

    /// Settings advertised by the MCP server.
    pub fn server_settings(&self) -> &Value {
        &self.server_settings
    }
}

/// Protocol extensions declared by both the client and server.
///
/// This is inserted into each [`RequestContext`](crate::RequestContext), so
/// handlers and per-capability middleware can make extension-specific policy
/// decisions without inspecting raw capability maps.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NegotiatedExtensions {
    extensions: BTreeMap<String, NegotiatedExtension>,
}

impl NegotiatedExtensions {
    /// Compute the exact identifier intersection of two capability maps.
    pub fn from_capabilities(client: &ClientCapabilities, server: &ServerCapabilities) -> Self {
        Self::from_maps(client.extensions.as_ref(), server.extensions.as_ref())
    }

    pub(crate) fn from_maps(
        client: Option<&HashMap<String, Value>>,
        server: Option<&HashMap<String, Value>>,
    ) -> Self {
        let mut extensions = BTreeMap::new();
        let (Some(client), Some(server)) = (client, server) else {
            return Self { extensions };
        };

        for (identifier, client_settings) in client {
            let Some(server_settings) = server.get(identifier) else {
                continue;
            };
            if validate_extension_identifier(identifier).is_err()
                || !client_settings.is_object()
                || !server_settings.is_object()
            {
                continue;
            }
            extensions.insert(
                identifier.clone(),
                NegotiatedExtension {
                    client_settings: client_settings.clone(),
                    server_settings: server_settings.clone(),
                },
            );
        }
        Self { extensions }
    }

    /// Return the negotiated declaration for an extension identifier.
    pub fn get(&self, identifier: &str) -> Option<&NegotiatedExtension> {
        self.extensions.get(identifier)
    }

    /// Return whether both peers declared an extension identifier.
    pub fn contains(&self, identifier: &str) -> bool {
        self.extensions.contains_key(identifier)
    }

    /// Iterate over negotiated extensions in identifier order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &NegotiatedExtension)> {
        self.extensions
            .iter()
            .map(|(identifier, extension)| (identifier.as_str(), extension))
    }

    /// Number of negotiated extension identifiers.
    pub fn len(&self) -> usize {
        self.extensions.len()
    }

    /// Return whether no extension was declared by both peers.
    pub fn is_empty(&self) -> bool {
        self.extensions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declaration_requires_vendor_prefix_and_object_settings() {
        assert!(ExtensionDeclaration::empty("com.example/feature").is_ok());
        assert!(matches!(
            ExtensionDeclaration::empty("feature"),
            Err(MetaValidationError::MissingExtensionPrefix(_))
        ));
        assert!(matches!(
            ExtensionDeclaration::new("com.example/feature", true),
            Err(MetaValidationError::InvalidExtensionSettings(_))
        ));
    }

    #[test]
    fn negotiation_is_the_exact_identifier_intersection() {
        let client = ClientCapabilities {
            extensions: Some(HashMap::from([
                (
                    "com.example/shared".to_string(),
                    serde_json::json!({"client": true}),
                ),
                ("com.example/client-only".to_string(), serde_json::json!({})),
            ])),
            ..ClientCapabilities::default()
        };
        let server = ServerCapabilities {
            extensions: Some(HashMap::from([
                (
                    "com.example/shared".to_string(),
                    serde_json::json!({"server": true}),
                ),
                ("com.example/server-only".to_string(), serde_json::json!({})),
            ])),
            ..ServerCapabilities::default()
        };

        let negotiated = NegotiatedExtensions::from_capabilities(&client, &server);

        assert_eq!(negotiated.len(), 1);
        let shared = negotiated.get("com.example/shared").unwrap();
        assert_eq!(shared.client_settings()["client"], true);
        assert_eq!(shared.server_settings()["server"], true);
        assert!(!negotiated.contains("com.example/client-only"));
        assert!(!negotiated.contains("com.example/server-only"));
    }
}
