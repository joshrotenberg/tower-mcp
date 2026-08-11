//! Task-oriented guides for building and operating tower-mcp applications.
//!
//! These guides complement the API reference and point to the authoritative
//! MCP specification for wire-protocol semantics:
//!
//! - [`client`] — transports, lifecycle, callbacks, requests, caching, retries,
//!   and shutdown.
//! - [`deployment`] — endpoint mounting, reverse proxies, origin and host
//!   policy, sessions, scaling, timeouts, health, and middleware order.
//! - [`protocol_versions`] — compile-time availability, runtime allowlists,
//!   lifecycle differences, interoperability, and upgrades.
//! - [`oauth`] — protected resource servers, interactive clients,
//!   service-to-service clients, persistence, and production policy.
//! - [`mcp_apps`] — typed MCP Apps resources, negotiation, fallback, CSP,
//!   permissions, and visibility.

// Each guide is gated on the features its examples use. The guides are
// doc-only, so this is invisible to callers, and it stops a default-features
// reader being shown a page of examples they cannot compile. docs.rs builds
// with all features, so every guide still renders there (#1275).
#[cfg(feature = "http-client")]
pub mod client;
#[cfg(feature = "http")]
pub mod deployment;
#[cfg(feature = "mcp-apps")]
pub mod mcp_apps;
#[cfg(feature = "oauth-client")]
pub mod oauth;
pub mod protocol_versions;
