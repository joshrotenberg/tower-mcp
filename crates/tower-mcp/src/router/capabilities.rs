//! Reading the capabilities and principal a request arrived with.
//!
//! The `final_*` and legacy spellings sit side by side here because the two
//! protocol revisions carry this information differently, and every caller
//! wants the same answer regardless.

use super::*;

pub(super) type TaskOwnerResolver =
    Arc<dyn Fn(&crate::context::Extensions) -> TaskOwnerResolution + Send + Sync + 'static>;

/// The result of resolving the caller that owns a Task.
///
/// `Invalid` is deliberately distinct from anonymous. Collapsing a broken
/// application resolver to `None` would let it match an unowned Task and turn
/// an authentication failure into authorization.
pub(super) enum TaskOwnerResolution {
    Resolved(crate::async_task::TaskOwner),
    Invalid,
}

impl TaskOwnerResolution {
    pub(super) fn into_owner(self) -> Option<crate::async_task::TaskOwner> {
        match self {
            Self::Resolved(owner) => Some(owner),
            Self::Invalid => None,
        }
    }

    pub(super) fn matches(&self, owner: &crate::async_task::TaskOwner) -> bool {
        match self {
            Self::Resolved(principal) => {
                crate::async_task::owner_matches(owner, principal.as_deref())
            }
            Self::Invalid => false,
        }
    }
}

/// Build the source-compatible default Task owner resolver.
///
/// OAuth subjects are deliberately copied verbatim. Existing deployments may
/// persist those strings outside this process, so prefixing or normalizing
/// them here would strand their Tasks.
pub(super) fn default_task_owner_resolver() -> TaskOwnerResolver {
    Arc::new(|extensions| TaskOwnerResolution::Resolved(oauth_task_owner(extensions)))
}

/// Wrap application mapping code in the Task authorization boundary.
pub(super) fn custom_task_owner_resolver<F>(resolver: F) -> TaskOwnerResolver
where
    F: Fn(&crate::context::Extensions) -> Option<String> + Send + Sync + 'static,
{
    Arc::new(move |extensions| {
        let resolved =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| resolver(extensions)));
        match resolved {
            Ok(Some(owner)) if !owner.trim().is_empty() => {
                TaskOwnerResolution::Resolved(Some(owner))
            }
            Ok(Some(_)) => {
                tracing::error!(
                    target: "mcp::tasks",
                    "task owner resolver returned an empty owner; denying the operation"
                );
                TaskOwnerResolution::Invalid
            }
            Ok(None) => TaskOwnerResolution::Resolved(None),
            Err(_) => {
                // Do not log the panic payload or extensions. Either may
                // contain credentials supplied to the application resolver.
                tracing::error!(
                    target: "mcp::tasks",
                    "task owner resolver panicked; denying the operation"
                );
                TaskOwnerResolution::Invalid
            }
        }
    })
}

/// The default authenticated Task owner for this request, if any.
#[cfg(feature = "oauth")]
fn oauth_task_owner(extensions: &crate::context::Extensions) -> Option<String> {
    extensions
        .get::<crate::oauth::token::TokenClaims>()
        .and_then(|claims| claims.sub.clone())
}

#[cfg(not(feature = "oauth"))]
fn oauth_task_owner(_extensions: &crate::context::Extensions) -> Option<String> {
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
