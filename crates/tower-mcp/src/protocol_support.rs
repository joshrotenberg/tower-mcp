//! Compile-time and runtime protocol-version policy.
//!
//! The types crate exposes every wire version it knows. This module answers a
//! different question: which implementations were compiled into `tower-mcp`,
//! and which subset should a particular transport advertise and accept?

use std::collections::HashSet;

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
use crate::protocol::EXPERIMENTAL_PROTOCOL_VERSION;
use crate::protocol::SUPPORTED_PROTOCOL_VERSIONS;

/// Protocol versions compiled into this build, in preference order.
///
/// Enabling `protocol-2026-07-28` adds the experimental implementation. The
/// former `stateless` feature remains a compatibility alias and produces the
/// same compiled set.
#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
pub const COMPILED_PROTOCOL_VERSIONS: &[&str] =
    &[EXPERIMENTAL_PROTOCOL_VERSION, "2025-11-25", "2025-03-26"];

/// Protocol versions compiled into this build, in preference order.
#[cfg(not(any(feature = "protocol-2026-07-28", feature = "stateless")))]
pub const COMPILED_PROTOCOL_VERSIONS: &[&str] = SUPPORTED_PROTOCOL_VERSIONS;

/// Returns whether a protocol implementation is present in this build.
pub fn is_protocol_version_compiled(version: &str) -> bool {
    COMPILED_PROTOCOL_VERSIONS.contains(&version)
}

/// Error returned for an invalid runtime protocol configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolSupportError {
    /// At least one protocol version must remain enabled.
    #[error("at least one protocol version must be enabled")]
    Empty,
    /// The requested version was not compiled into this build.
    #[error(
        "protocol version `{version}` is not compiled into this build; compiled versions: {compiled:?}"
    )]
    NotCompiled {
        /// Version requested by the application.
        version: String,
        /// Versions available in this build.
        compiled: &'static [&'static str],
    },
    /// A version appeared more than once in the preference list.
    #[error("protocol version `{0}` is configured more than once")]
    Duplicate(String),
}

/// Exact, ordered protocol-version allow-list for one runtime component.
///
/// [`ProtocolSupport::default`] enables every version compiled into the crate.
/// Use [`ProtocolSupport::try_new`] to narrow an individual server or client.
/// The order is preserved and is used as the advertised preference order.
///
/// ```
/// use tower_mcp::ProtocolSupport;
///
/// let support = ProtocolSupport::try_new(["2025-11-25"])?;
/// assert!(support.contains("2025-11-25"));
/// assert!(!support.contains("2025-03-26"));
/// # Ok::<(), tower_mcp::ProtocolSupportError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolSupport {
    versions: Vec<String>,
}

impl ProtocolSupport {
    /// Enable every protocol implementation compiled into this build.
    pub fn compiled() -> Self {
        Self {
            versions: COMPILED_PROTOCOL_VERSIONS
                .iter()
                .map(|version| (*version).to_string())
                .collect(),
        }
    }

    /// Enable only the stable-by-default protocol versions.
    ///
    /// This excludes experimental implementations even when their Cargo
    /// features are compiled.
    pub fn stable() -> Self {
        Self {
            versions: SUPPORTED_PROTOCOL_VERSIONS
                .iter()
                .map(|version| (*version).to_string())
                .collect(),
        }
    }

    /// Construct an exact runtime allow-list.
    ///
    /// Versions must be compiled into the crate, must not be duplicated, and
    /// the list must not be empty.
    pub fn try_new<I, S>(versions: I) -> Result<Self, ProtocolSupportError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut configured = Vec::new();
        let mut seen = HashSet::new();

        for version in versions {
            let version = version.into();
            if !is_protocol_version_compiled(&version) {
                return Err(ProtocolSupportError::NotCompiled {
                    version,
                    compiled: COMPILED_PROTOCOL_VERSIONS,
                });
            }
            if !seen.insert(version.clone()) {
                return Err(ProtocolSupportError::Duplicate(version));
            }
            configured.push(version);
        }

        if configured.is_empty() {
            return Err(ProtocolSupportError::Empty);
        }

        Ok(Self {
            versions: configured,
        })
    }

    /// Enabled versions in advertised preference order.
    pub fn versions(&self) -> &[String] {
        &self.versions
    }

    /// Whether this runtime component should accept a version.
    pub fn contains(&self, version: &str) -> bool {
        self.versions.iter().any(|candidate| candidate == version)
    }

    /// Most-preferred enabled version.
    pub fn preferred(&self) -> &str {
        // Construction and the two built-in policies guarantee non-empty.
        &self.versions[0]
    }
}

impl Default for ProtocolSupport {
    fn default() -> Self {
        Self::compiled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_policy_excludes_experimental_versions() {
        let support = ProtocolSupport::stable();
        assert_eq!(support.versions(), SUPPORTED_PROTOCOL_VERSIONS);
        assert_eq!(support.preferred(), "2025-11-25");
    }

    #[test]
    fn rejects_empty_duplicate_and_uncompiled_sets() {
        assert_eq!(
            ProtocolSupport::try_new(Vec::<String>::new()).unwrap_err(),
            ProtocolSupportError::Empty
        );
        assert_eq!(
            ProtocolSupport::try_new(["2025-11-25", "2025-11-25"]).unwrap_err(),
            ProtocolSupportError::Duplicate("2025-11-25".to_string())
        );
        assert!(matches!(
            ProtocolSupport::try_new(["2099-01-01"]).unwrap_err(),
            ProtocolSupportError::NotCompiled { .. }
        ));
    }

    #[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
    #[test]
    fn experimental_feature_adds_2026_implementation() {
        assert!(is_protocol_version_compiled(EXPERIMENTAL_PROTOCOL_VERSION));
        assert!(ProtocolSupport::compiled().contains(EXPERIMENTAL_PROTOCOL_VERSION));
    }

    #[cfg(not(any(feature = "protocol-2026-07-28", feature = "stateless")))]
    #[test]
    fn default_build_does_not_compile_2026_implementation() {
        assert!(!is_protocol_version_compiled("2026-07-28"));
    }
}
