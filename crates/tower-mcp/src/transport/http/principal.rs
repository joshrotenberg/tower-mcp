//! Binding an HTTP session to the principal that created it (#1515).
//!
//! A request that carries `MCP-Session-Id` used to be routed to that session
//! by ID alone, so a leaked or guessed ID was enough for a second caller to
//! use, read the stream of, or terminate someone else's session. Each session
//! now records the principal resolved when it was created, and every later
//! request that names it must resolve to the same principal.
//!
//! The resolution semantics deliberately mirror the Task owner resolver in
//! `router::capabilities`: the default is the OAuth subject copied verbatim,
//! an application resolver is wrapped in `catch_unwind`, and a panic or an
//! empty principal is an [`SessionPrincipal::Invalid`] resolution that never
//! matches, which is distinct from an anonymous caller. The matching rule is
//! the Task one too ([`owner_matches`](crate::async_task::owner_matches)).
//! The resolver wrapper is a parallel copy rather than a shared one because
//! the Task version is private to the router and names Tasks in its log
//! output.

use super::*;

pub(super) type SessionPrincipalResolver =
    Arc<dyn Fn(&crate::context::Extensions) -> SessionPrincipal + Send + Sync + 'static>;

/// The result of resolving the caller behind an HTTP request.
///
/// `Invalid` is deliberately distinct from anonymous. Collapsing a broken
/// application resolver to `None` would let it match an unbound session and
/// turn an authentication failure into authorization.
pub(super) enum SessionPrincipal {
    Resolved(Option<String>),
    Invalid,
}

impl SessionPrincipal {
    /// The principal to bind a new session to, or `None` when the resolver
    /// failed and the session must not be created.
    pub(super) fn into_bound(self) -> Option<Option<String>> {
        match self {
            Self::Resolved(principal) => Some(principal),
            Self::Invalid => None,
        }
    }

    /// Whether this request may use a session bound to `bound`.
    pub(super) fn matches(&self, bound: &Option<String>) -> bool {
        match self {
            Self::Resolved(principal) => {
                crate::async_task::owner_matches(bound, principal.as_deref())
            }
            Self::Invalid => false,
        }
    }
}

/// Build the default session principal resolver.
///
/// This is the same mapping the router uses for Task owners by default: the
/// OAuth subject, copied verbatim. Without the `oauth` feature every request
/// is anonymous and sessions behave as they did before binding existed.
pub(super) fn default_session_principal_resolver() -> SessionPrincipalResolver {
    Arc::new(|extensions| SessionPrincipal::Resolved(crate::router::oauth_task_owner(extensions)))
}

/// Wrap application mapping code in the session authorization boundary.
pub(super) fn custom_session_principal_resolver<F>(resolver: F) -> SessionPrincipalResolver
where
    F: Fn(&crate::context::Extensions) -> Option<String> + Send + Sync + 'static,
{
    Arc::new(move |extensions| {
        let resolved =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| resolver(extensions)));
        match resolved {
            Ok(Some(principal)) if !principal.trim().is_empty() => {
                SessionPrincipal::Resolved(Some(principal))
            }
            Ok(Some(_)) => {
                tracing::error!(
                    target: "mcp::session",
                    "session principal resolver returned an empty principal; denying the request"
                );
                SessionPrincipal::Invalid
            }
            Ok(None) => SessionPrincipal::Resolved(None),
            Err(_) => {
                // Do not log the panic payload or extensions. Either may
                // contain credentials supplied to the application resolver.
                tracing::error!(
                    target: "mcp::session",
                    "session principal resolver panicked; denying the request"
                );
                SessionPrincipal::Invalid
            }
        }
    })
}

/// Resolve the principal for one HTTP request.
///
/// The resolver sees the extensions the request is served with: OAuth
/// `TokenClaims` when that feature is compiled in, plus every type registered
/// with [`HttpTransport::bridge_extension`].
pub(super) fn resolve_request_principal(
    state: &AppState,
    http_extensions: &axum::http::Extensions,
) -> SessionPrincipal {
    let mut extensions = crate::context::Extensions::new();
    #[cfg(feature = "oauth")]
    if let Some(claims) = http_extensions.get::<crate::oauth::token::TokenClaims>() {
        extensions.insert(claims.clone());
    }
    crate::transport::extension_bridge::apply_extension_bridges(
        &state.extension_bridges,
        http_extensions,
        &mut extensions,
    );
    (state.session_principal_resolver)(&extensions)
}
