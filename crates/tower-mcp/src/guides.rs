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

pub mod client;
pub mod deployment;
pub mod mcp_apps;
pub mod oauth;
pub mod protocol_versions;
