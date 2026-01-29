//! Secret string handling for sensitive values.
//!
//! This module provides [`SecretString`], a wrapper type that prevents accidental
//! exposure of sensitive values like API keys and tokens in logs, debug output,
//! and error messages.
//!
//! # Example
//!
//! ```rust
//! use tower_mcp::SecretString;
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(Debug, Deserialize, JsonSchema)]
//! struct GitHubInput {
//!     org: String,
//!     token: SecretString,  // Safe to debug/log the entire struct
//! }
//!
//! fn use_token(input: GitHubInput) {
//!     // Debug output shows "[REDACTED]" for the token
//!     println!("{:?}", input);
//!
//!     // Explicitly expose the secret when needed
//!     let token_value = input.token.expose();
//!     // ... use token_value with external API
//! }
//! ```
//!
//! # Custom Labels
//!
//! You can customize the redaction label for more descriptive output:
//!
//! ```rust
//! use tower_mcp::SecretString;
//!
//! let api_key = SecretString::with_label("sk-1234", "API_KEY");
//! let token = SecretString::with_label("ghp_xxx", "GITHUB_TOKEN");
//!
//! assert_eq!(format!("{:?}", api_key), "[API_KEY]");
//! assert_eq!(format!("{:?}", token), "[GITHUB_TOKEN]");
//! ```

use std::borrow::Cow;
use std::fmt::{self, Debug, Display, Formatter};

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A string wrapper that redacts its contents in Debug and Display output.
///
/// Use this type for API keys, tokens, passwords, and other sensitive values
/// in tool input types. The value is preserved internally but hidden from
/// accidental exposure through logging, debug output, or error messages.
///
/// # Serialization
///
/// `SecretString` serializes and deserializes transparently as a plain string,
/// making it compatible with JSON schema generation and MCP tool inputs.
///
/// # Accessing the Value
///
/// Use [`expose()`](SecretString::expose) to access the underlying value when
/// you need to use it (e.g., passing to an external API).
///
/// # Custom Labels
///
/// By default, Debug/Display output shows `[REDACTED]`. Use [`with_label()`](SecretString::with_label)
/// to customize this for more descriptive output:
///
/// ```rust
/// use tower_mcp::SecretString;
///
/// // Default label
/// let secret = SecretString::new("my-api-key");
/// assert_eq!(format!("{:?}", secret), "[REDACTED]");
///
/// // Custom label
/// let api_key = SecretString::with_label("my-api-key", "API_KEY");
/// assert_eq!(format!("{:?}", api_key), "[API_KEY]");
///
/// // Both expose the same value
/// assert_eq!(secret.expose(), "my-api-key");
/// assert_eq!(api_key.expose(), "my-api-key");
/// ```
#[derive(Clone)]
pub struct SecretString {
    value: String,
    label: Cow<'static, str>,
}

const DEFAULT_LABEL: &str = "REDACTED";

impl SecretString {
    /// Create a new `SecretString` from any string-like value.
    ///
    /// The default redaction label is `[REDACTED]`. Use [`with_label()`](SecretString::with_label)
    /// for a custom label.
    pub fn new(s: impl Into<String>) -> Self {
        Self {
            value: s.into(),
            label: Cow::Borrowed(DEFAULT_LABEL),
        }
    }

    /// Create a new `SecretString` with a custom redaction label.
    ///
    /// The label appears in Debug/Display output as `[LABEL]`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::SecretString;
    ///
    /// let secret = SecretString::with_label("ghp_xxxx", "GITHUB_TOKEN");
    /// assert_eq!(format!("{}", secret), "[GITHUB_TOKEN]");
    /// assert_eq!(secret.expose(), "ghp_xxxx");
    /// ```
    pub fn with_label(s: impl Into<String>, label: impl Into<Cow<'static, str>>) -> Self {
        Self {
            value: s.into(),
            label: label.into(),
        }
    }

    /// Expose the underlying secret value.
    ///
    /// Use this method when you need to actually use the secret, such as
    /// passing it to an external API. Be careful not to log or display
    /// the returned value.
    pub fn expose(&self) -> &str {
        &self.value
    }

    /// Consume the `SecretString` and return the underlying value.
    ///
    /// Use this when you need ownership of the secret string.
    pub fn into_inner(self) -> String {
        self.value
    }

    /// Returns the redaction label.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Returns true if the secret string is empty.
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// Returns the length of the secret string in bytes.
    pub fn len(&self) -> usize {
        self.value.len()
    }
}

impl Default for SecretString {
    fn default() -> Self {
        Self {
            value: String::new(),
            label: Cow::Borrowed(DEFAULT_LABEL),
        }
    }
}

impl Debug for SecretString {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "[{}]", self.label)
    }
}

impl Display for SecretString {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "[{}]", self.label)
    }
}

impl PartialEq for SecretString {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for SecretString {}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<&str> for SecretString {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

// Custom serde implementation to serialize/deserialize as plain string
impl Serialize for SecretString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(Self::new(value))
    }
}

// Custom JsonSchema implementation to produce a simple string schema
impl JsonSchema for SecretString {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SecretString")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        // SecretString is just a string in JSON
        serde_json::json!({
            "type": "string"
        })
        .try_into()
        .expect("valid schema")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debug_is_redacted() {
        let secret = SecretString::new("my-secret-key");
        assert_eq!(format!("{:?}", secret), "[REDACTED]");
    }

    #[test]
    fn test_display_is_redacted() {
        let secret = SecretString::new("my-secret-key");
        assert_eq!(format!("{}", secret), "[REDACTED]");
    }

    #[test]
    fn test_custom_label_debug() {
        let secret = SecretString::with_label("my-api-key", "API_KEY");
        assert_eq!(format!("{:?}", secret), "[API_KEY]");
    }

    #[test]
    fn test_custom_label_display() {
        let secret = SecretString::with_label("ghp_xxxx", "GITHUB_TOKEN");
        assert_eq!(format!("{}", secret), "[GITHUB_TOKEN]");
    }

    #[test]
    fn test_custom_label_owned_string() {
        let label = String::from("DYNAMIC_LABEL");
        let secret = SecretString::with_label("value", label);
        assert_eq!(format!("{}", secret), "[DYNAMIC_LABEL]");
    }

    #[test]
    fn test_label_accessor() {
        let secret = SecretString::with_label("value", "MY_LABEL");
        assert_eq!(secret.label(), "MY_LABEL");

        let default = SecretString::new("value");
        assert_eq!(default.label(), "REDACTED");
    }

    #[test]
    fn test_expose_returns_value() {
        let secret = SecretString::new("my-secret-key");
        assert_eq!(secret.expose(), "my-secret-key");
    }

    #[test]
    fn test_expose_with_custom_label() {
        let secret = SecretString::with_label("my-secret-key", "CUSTOM");
        assert_eq!(secret.expose(), "my-secret-key");
    }

    #[test]
    fn test_into_inner() {
        let secret = SecretString::new("my-secret-key");
        assert_eq!(secret.into_inner(), "my-secret-key");
    }

    #[test]
    fn test_clone_preserves_value_and_label() {
        let secret = SecretString::with_label("my-secret-key", "CLONED");
        let cloned = secret.clone();
        assert_eq!(cloned.expose(), "my-secret-key");
        assert_eq!(cloned.label(), "CLONED");
    }

    #[test]
    fn test_equality() {
        let s1 = SecretString::new("same");
        let s2 = SecretString::new("same");
        let s3 = SecretString::new("different");
        assert_eq!(s1, s2);
        assert_ne!(s1, s3);
    }

    #[test]
    fn test_equality_ignores_label() {
        // Two secrets with same value but different labels should be equal
        let s1 = SecretString::new("same");
        let s2 = SecretString::with_label("same", "CUSTOM");
        assert_eq!(s1, s2);
    }

    #[test]
    fn test_from_string() {
        let s: SecretString = String::from("test").into();
        assert_eq!(s.expose(), "test");
    }

    #[test]
    fn test_from_str() {
        let s: SecretString = "test".into();
        assert_eq!(s.expose(), "test");
    }

    #[test]
    fn test_default_is_empty() {
        let s = SecretString::default();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert_eq!(s.label(), "REDACTED");
    }

    #[test]
    fn test_len_and_is_empty() {
        let s = SecretString::new("hello");
        assert!(!s.is_empty());
        assert_eq!(s.len(), 5);
    }

    #[test]
    fn test_serde_roundtrip() {
        let secret = SecretString::new("my-api-key");
        let json = serde_json::to_string(&secret).unwrap();
        assert_eq!(json, "\"my-api-key\"");

        let parsed: SecretString = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.expose(), "my-api-key");
    }

    #[test]
    fn test_serde_with_label_serializes_value_only() {
        let secret = SecretString::with_label("my-api-key", "CUSTOM");
        let json = serde_json::to_string(&secret).unwrap();
        // Label is not serialized - only the value
        assert_eq!(json, "\"my-api-key\"");
    }

    #[test]
    fn test_serde_deserialize_gets_default_label() {
        let parsed: SecretString = serde_json::from_str("\"some-value\"").unwrap();
        assert_eq!(parsed.expose(), "some-value");
        assert_eq!(parsed.label(), "REDACTED"); // Default label after deserialization
    }

    #[test]
    fn test_json_schema() {
        let schema = schemars::schema_for!(SecretString);
        let json = serde_json::to_string_pretty(&schema).unwrap();
        // Should generate a simple string schema
        assert!(json.contains("\"type\": \"string\""));
    }

    #[test]
    fn test_struct_with_secret_debug() {
        #[allow(dead_code)]
        #[derive(Debug)]
        struct Config {
            name: String,
            api_key: SecretString,
        }

        let config = Config {
            name: "test".to_string(),
            api_key: SecretString::new("super-secret"),
        };

        let debug_output = format!("{:?}", config);
        assert!(debug_output.contains("test"));
        assert!(debug_output.contains("[REDACTED]"));
        assert!(!debug_output.contains("super-secret"));
    }

    #[test]
    fn test_struct_with_labeled_secret_debug() {
        #[allow(dead_code)]
        #[derive(Debug)]
        struct Config {
            name: String,
            api_key: SecretString,
            github_token: SecretString,
        }

        let config = Config {
            name: "test".to_string(),
            api_key: SecretString::with_label("sk-xxxx", "API_KEY"),
            github_token: SecretString::with_label("ghp_yyyy", "GITHUB_TOKEN"),
        };

        let debug_output = format!("{:?}", config);
        assert!(debug_output.contains("test"));
        assert!(debug_output.contains("[API_KEY]"));
        assert!(debug_output.contains("[GITHUB_TOKEN]"));
        assert!(!debug_output.contains("sk-xxxx"));
        assert!(!debug_output.contains("ghp_yyyy"));
    }
}
