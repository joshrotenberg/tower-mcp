//! Bridging server-supplied request extensions into MCP requests.
//!
//! A tower layer in front of an HTTP or WebSocket transport can attach
//! anything it likes to the request: a resolved identity, a tenant, a
//! tracing id, the peer address. None of it reached the handler, because
//! the transports built the per-request [`Extensions`] from scratch and
//! lifted exactly one type out of the HTTP request (OAuth `TokenClaims`).
//!
//! Registering a type with `bridge_extension` copies it across, where
//! [`RequestContext::extension`](crate::RequestContext::extension) can find
//! it (#1242).
//!
//! Bridging is opt-in per type rather than wholesale. The alternative,
//! carrying the whole HTTP extension map over, would put an
//! `axum::http::Extensions` in front of handlers that are otherwise
//! transport-agnostic, and would move data across the boundary that the
//! server never decided to expose.

use std::sync::Arc;

use crate::context::Extensions;

/// Copies one registered type from an HTTP request's extensions into the
/// per-request MCP extensions.
///
/// Type-erased so a transport can hold a list of them without naming the
/// types it was asked to bridge.
pub(crate) type ExtensionBridge =
    Arc<dyn Fn(&axum::http::Extensions, &mut Extensions) + Send + Sync>;

/// Build a bridge for `T`.
///
/// A request that carries no `T` is left alone rather than treated as an
/// error: a layer that only attaches an identity to some routes is a normal
/// configuration, and the handler already has to treat the extension as
/// optional.
pub(crate) fn extension_bridge<T>() -> ExtensionBridge
where
    T: Clone + Send + Sync + 'static,
{
    Arc::new(|from, to| {
        if let Some(value) = from.get::<T>() {
            to.insert(value.clone());
        }
    })
}

/// Run every registered bridge for one request.
pub(crate) fn apply_extension_bridges(
    bridges: &[ExtensionBridge],
    from: &axum::http::Extensions,
    to: &mut Extensions,
) {
    for bridge in bridges {
        bridge(from, to);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct Identity(&'static str);

    #[derive(Debug, Clone, PartialEq)]
    struct TraceId(u32);

    #[derive(Debug, Clone, PartialEq)]
    struct NeverRegistered;

    #[test]
    fn a_registered_type_crosses_and_an_unregistered_one_does_not() {
        let mut from = axum::http::Extensions::new();
        from.insert(Identity("agent-7"));
        from.insert(NeverRegistered);

        let mut to = Extensions::new();
        apply_extension_bridges(&[extension_bridge::<Identity>()], &from, &mut to);

        assert_eq!(to.get::<Identity>(), Some(&Identity("agent-7")));
        assert!(
            to.get::<NeverRegistered>().is_none(),
            "only registered types cross the boundary"
        );
    }

    /// A request missing the type is not an error; the handler already has
    /// to treat the extension as optional.
    #[test]
    fn a_missing_type_is_skipped() {
        let from = axum::http::Extensions::new();
        let mut to = Extensions::new();
        apply_extension_bridges(&[extension_bridge::<Identity>()], &from, &mut to);
        assert!(to.get::<Identity>().is_none());
    }

    #[test]
    fn every_registered_type_is_applied() {
        let mut from = axum::http::Extensions::new();
        from.insert(Identity("agent-7"));
        from.insert(TraceId(42));

        let mut to = Extensions::new();
        apply_extension_bridges(
            &[extension_bridge::<Identity>(), extension_bridge::<TraceId>()],
            &from,
            &mut to,
        );

        assert_eq!(to.get::<Identity>(), Some(&Identity("agent-7")));
        assert_eq!(to.get::<TraceId>(), Some(&TraceId(42)));
    }
}
