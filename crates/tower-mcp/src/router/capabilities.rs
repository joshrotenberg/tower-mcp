//! Reading the capabilities and principal a request arrived with, and
//! deciding which server capabilities this router advertises.
//!
//! The `final_*` and legacy spellings sit side by side here because the two
//! protocol revisions carry this information differently, and every caller
//! wants the same answer regardless.

use super::*;

/// A server capability whose advertisement decides whether a set of request
/// methods is routed at all (#1654).
///
/// The lifecycle on both protocol revisions says a peer uses only the
/// capabilities the other side declared. A router that does not advertise a
/// capability therefore answers its methods with `-32601` (method not found)
/// rather than an empty success, which would contradict what `initialize` and
/// `server/discover` just told the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScopedCapability {
    /// `capabilities.tools`: `tools/list`, `tools/call`.
    Tools,
    /// `capabilities.resources`: `resources/list`, `resources/templates/list`,
    /// `resources/read`, `resources/subscribe`, `resources/unsubscribe`.
    Resources,
    /// `capabilities.prompts`: `prompts/list`, `prompts/get`.
    Prompts,
    /// `capabilities.completions`: `completion/complete`.
    Completions,
}

impl ScopedCapability {
    /// The capability a request requires, or `None` for a method that is not
    /// scoped by one of the four registry-derived capabilities.
    ///
    /// Deliberately absent:
    ///
    /// - `initialize`, `server/discover`, `ping`, and `subscriptions/listen`
    ///   are lifecycle methods every server answers.
    /// - `logging/setLevel` stays routed whether or not `logging` is
    ///   advertised. That capability follows the notification channel a
    ///   transport attaches, so a router used directly would start refusing a
    ///   harmless request it has always accepted. The method is deprecated
    ///   (SEP-2577) and already refused on the 2026-07-28 lifecycle.
    /// - The `tasks/*` methods carry their own negotiation check.
    /// - `skills/list`, `skills/get`, and `resources/directory/read` answer
    ///   `-32601` themselves when no skills catalog is configured, and a
    ///   catalog always advertises `resources`.
    ///
    /// `resources/subscribe` and `resources/unsubscribe` require `resources`
    /// only, not `resources.subscribe`: [`McpRouter::resource_subscriptions`]
    /// is documented to affect advertisement alone.
    pub(super) fn required_by(request: &McpRequest) -> Option<Self> {
        match request {
            McpRequest::ListTools(_) | McpRequest::CallTool(_) => Some(Self::Tools),
            McpRequest::ListResources(_)
            | McpRequest::ListResourceTemplates(_)
            | McpRequest::ReadResource(_)
            | McpRequest::SubscribeResource(_)
            | McpRequest::UnsubscribeResource(_) => Some(Self::Resources),
            McpRequest::ListPrompts(_) | McpRequest::GetPrompt(_) => Some(Self::Prompts),
            McpRequest::Complete(_) => Some(Self::Completions),
            _ => None,
        }
    }
}

impl McpRouter {
    /// Whether this router advertises `capability`.
    ///
    /// This is the single predicate behind both the capability object sent in
    /// `initialize` / `server/discover` and the dispatch gate in `handle`, so
    /// the two cannot disagree. Neither protocol revision changes the answer:
    /// `capabilities_for_protocol` adjusts flags inside these capabilities,
    /// never their presence.
    ///
    /// A configured dynamic registry advertises its capability even while it
    /// is empty. Entries can be registered at any time after `initialize`, and
    /// the capability set a client received then is not renegotiated, so the
    /// methods must stay routable for the life of the router. A lazy
    /// initializer such as `dynamic_prompt_initializer` advertises nothing by
    /// itself; it fills the dynamic registry, which does.
    ///
    /// A skills catalog advertises `resources`, as SEP-2640 requires, even
    /// when every skill is dynamic and has no resource registered here.
    pub(super) fn advertises(&self, capability: ScopedCapability) -> bool {
        match capability {
            ScopedCapability::Tools => {
                #[cfg(feature = "dynamic-tools")]
                let dynamic = self.inner.dynamic_tools.is_some();
                #[cfg(not(feature = "dynamic-tools"))]
                let dynamic = false;
                !self.inner.tools.is_empty() || dynamic
            }
            ScopedCapability::Resources => {
                #[cfg(feature = "dynamic-tools")]
                let dynamic = self.inner.dynamic_resources.is_some()
                    || self.inner.dynamic_resource_templates.is_some();
                #[cfg(not(feature = "dynamic-tools"))]
                let dynamic = false;
                #[cfg(feature = "skills")]
                let skills = self.inner.skills.is_some();
                #[cfg(not(feature = "skills"))]
                let skills = false;
                !self.inner.resources.is_empty()
                    || !self.inner.resource_templates.is_empty()
                    || dynamic
                    || skills
            }
            ScopedCapability::Prompts => {
                #[cfg(feature = "dynamic-tools")]
                let dynamic = self.inner.dynamic_prompts.is_some();
                #[cfg(not(feature = "dynamic-tools"))]
                let dynamic = false;
                !self.inner.prompts.is_empty() || dynamic
            }
            ScopedCapability::Completions => self.inner.completion_handler.is_some(),
        }
    }
}

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
///
/// Also the default HTTP session principal, so a session and the Tasks created
/// in it are attributed to the same caller unless an application overrides
/// one of the two resolvers.
#[cfg(feature = "oauth")]
pub(crate) fn oauth_task_owner(extensions: &crate::context::Extensions) -> Option<String> {
    extensions
        .get::<crate::oauth::token::TokenClaims>()
        .and_then(|claims| claims.sub.clone())
}

#[cfg(not(feature = "oauth"))]
pub(crate) fn oauth_task_owner(_extensions: &crate::context::Extensions) -> Option<String> {
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
