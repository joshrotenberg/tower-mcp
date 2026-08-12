//! Reading the capabilities and principal a request arrived with.
//!
//! The `final_*` and legacy spellings sit side by side here because the two
//! protocol revisions carry this information differently, and every caller
//! wants the same answer regardless.

use super::*;

/// The authenticated principal for this request, if any.
///
/// Sourced from the OAuth `sub` claim that the HTTP and WebSocket transports
/// bridge into MCP extensions. Without the `oauth` feature there is no
/// principal, so tasks are unowned and behave as they did before ownership
/// existed.
#[cfg(feature = "oauth")]
pub(super) fn request_principal(extensions: &crate::context::Extensions) -> Option<String> {
    extensions
        .get::<crate::oauth::token::TokenClaims>()
        .and_then(|claims| claims.sub.clone())
}

#[cfg(not(feature = "oauth"))]
pub(super) fn request_principal(_extensions: &crate::context::Extensions) -> Option<String> {
    None
}

#[cfg(feature = "stateless")]
pub(super) fn final_client_capabilities(
    extensions: &crate::context::Extensions,
) -> Option<&ClientCapabilities> {
    extensions
        .get::<crate::stateless::StatelessRequestMeta>()
        .and_then(|meta| meta.client_capabilities.as_ref())
}

#[cfg(not(feature = "stateless"))]
pub(super) fn final_client_capabilities(
    _extensions: &crate::context::Extensions,
) -> Option<&ClientCapabilities> {
    None
}

/// Return whether `actual` contains every field and value in `required`.
///
/// Client capability objects are extensible, so extra advertised properties
/// must not cause a required-capability check to fail.
#[cfg(feature = "stateless")]
pub(super) fn json_value_contains(
    actual: &serde_json::Value,
    required: &serde_json::Value,
) -> bool {
    match (actual, required) {
        (serde_json::Value::Object(actual), serde_json::Value::Object(required)) => {
            required.iter().all(|(key, value)| {
                actual
                    .get(key)
                    .is_some_and(|a| json_value_contains(a, value))
            })
        }
        _ => actual == required,
    }
}

#[cfg(feature = "stateless")]
pub(super) fn client_capabilities_satisfy(
    actual: &ClientCapabilities,
    required: &ClientCapabilities,
) -> bool {
    let actual = serde_json::to_value(actual).expect("ClientCapabilities is always serializable");
    let mut required =
        serde_json::to_value(required).expect("ClientCapabilities is always serializable");
    // `roots.listChanged: false` means the optional notification capability
    // was not declared; it is not a requirement that the caller also set the
    // flag to false. Normalize it away before doing the structural subset
    // comparison so `{roots:{listChanged:true}}` satisfies plain `{roots:{}}`.
    if required.pointer("/roots/listChanged") == Some(&serde_json::Value::Bool(false))
        && let Some(roots) = required
            .get_mut("roots")
            .and_then(serde_json::Value::as_object_mut)
    {
        roots.remove("listChanged");
    }
    json_value_contains(&actual, &required)
}
