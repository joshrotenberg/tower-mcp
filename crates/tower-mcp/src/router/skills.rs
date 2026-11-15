//! Skills extension (SEP-2640) registration and dispatch for
//! [`McpRouter`](super::McpRouter).
//!
//! The catalog type and its loading rules live in [`crate::skills`]; this
//! module wires a [`SkillSet`] into the router and answers `skills/list`,
//! `skills/get`, and `resources/directory/read`.

use super::*;
use crate::skills::{
    GetSkillParams, GetSkillResult, ListSkillsParams, ListSkillsResult,
    METHOD_RESOURCES_DIRECTORY_READ, METHOD_SKILLS_GET, METHOD_SKILLS_LIST,
    ReadResourceDirectoryParams, ReadResourceDirectoryResult, SkillSet,
};

impl McpRouter {
    /// Serve a catalog of skills through the Skills extension (SEP-2640).
    ///
    /// Registers every skill file as a resource, answers `skills/list` and
    /// `skills/get` (and `resources/directory/read` when the catalog enables
    /// it), and declares `io.modelcontextprotocol/skills` together with the
    /// `resources` capability. Calling this again merges the new catalog into
    /// the existing one; a skill with the same URI is replaced.
    ///
    /// See [`crate::skills`] for how a catalog is built.
    ///
    /// ```rust
    /// use tower_mcp::McpRouter;
    /// use tower_mcp::skills::{SkillBuilder, SkillSet};
    ///
    /// # fn main() -> Result<(), tower_mcp::skills::SkillError> {
    /// let skills = SkillSet::new().skill(
    ///     SkillBuilder::new("git-workflow").skill_md(
    ///         "---\nname: git-workflow\ndescription: Follow the team's Git conventions\n---\n",
    ///     ),
    /// )?;
    /// let router = McpRouter::new().server_info("docs", "1.0.0").skills(skills);
    /// # let _ = router;
    /// # Ok(())
    /// # }
    /// ```
    pub fn skills(mut self, skills: SkillSet) -> Self {
        for resource in skills.resources() {
            self = self.resource(resource);
        }
        let inner = Arc::make_mut(&mut self.inner);
        let merged = match inner.skills.take() {
            Some(existing) => Arc::unwrap_or_clone(existing).merge(skills),
            None => skills,
        };
        let (identifier, settings) = merged.extension().into_parts();
        inner.protocol_extensions.insert(identifier, settings);
        inner.skills = Some(Arc::new(merged));
        self
    }

    /// Whether the `SKILL.md` resource at `uri` is visible to this request.
    ///
    /// A skill inherits the visibility of its manifest: a disabled or
    /// filtered-out `SKILL.md` hides the skill from `skills/list` and
    /// `skills/get`. A dynamic skill's manifest may be served by a template
    /// or dynamic registry rather than a static resource, in which case only
    /// the disabled set applies.
    fn skill_resource_visible(
        &self,
        filter_context: &CapabilityFilterContext<'_>,
        uri: &str,
    ) -> bool {
        if self.inner.disabled_resources.read().unwrap().contains(uri) {
            return false;
        }
        match (
            self.inner.resources.get(uri),
            self.inner.resource_filter.as_ref(),
        ) {
            (Some(resource), Some(filter)) => {
                filter.is_visible_with_context(filter_context, resource)
            }
            _ => true,
        }
    }

    fn skills_cache_hints(&self, skills: &SkillSet, router_ttl: Option<u64>) -> (u64, CacheScope) {
        // SEP-2640 makes both hints required on every skills result, so fall
        // back to "do not reuse" (0 ms, private) rather than omitting them.
        let ttl = skills.configured_ttl_ms().or(router_ttl).unwrap_or(0);
        let scope = skills
            .configured_cache_scope()
            .or(self.inner.cache_scope)
            .unwrap_or(CacheScope::Private);
        (ttl, scope)
    }

    pub(super) fn handle_list_skills(
        &self,
        params: ListSkillsParams,
        extensions: &Extensions,
    ) -> Result<McpResponse> {
        let Some(skills) = self.inner.skills.clone() else {
            return Err(Error::JsonRpc(JsonRpcError::method_not_found(
                METHOD_SKILLS_LIST,
            )));
        };
        let filter_context = self.capability_filter_context(extensions, CapabilityOperation::List);
        let entries: Vec<_> = skills
            .listed_skills()
            .filter(|skill| self.skill_resource_visible(&filter_context, &skill.uri))
            .cloned()
            .collect();
        let (entries, next_cursor) =
            paginate(entries, params.cursor.as_deref(), self.inner.page_size)?;
        let (ttl_ms, cache_scope) = self.skills_cache_hints(&skills, self.inner.list_ttl_ms);
        Ok(McpResponse::ListSkills(ListSkillsResult {
            skills: entries,
            next_cursor,
            ttl_ms: Some(ttl_ms),
            cache_scope: Some(cache_scope),
            meta: None,
        }))
    }

    pub(super) fn handle_get_skill(
        &self,
        params: GetSkillParams,
        extensions: &Extensions,
    ) -> Result<McpResponse> {
        let Some(skills) = self.inner.skills.clone() else {
            return Err(Error::JsonRpc(JsonRpcError::method_not_found(
                METHOD_SKILLS_GET,
            )));
        };
        let filter_context = self.capability_filter_context(
            extensions,
            CapabilityOperation::Access {
                target: &params.uri,
            },
        );
        let skill = skills
            .served_skill(&params.uri)
            .map(|(skill, _listed)| skill)
            .filter(|skill| self.skill_resource_visible(&filter_context, &skill.uri))
            .ok_or_else(|| {
                Error::invalid_params(format!("No skill is served at {}", params.uri))
            })?;
        let (ttl_ms, cache_scope) = self.skills_cache_hints(&skills, self.inner.read_ttl_ms);
        Ok(McpResponse::GetSkill(GetSkillResult {
            skill: skill.clone(),
            ttl_ms: Some(ttl_ms),
            cache_scope: Some(cache_scope),
            meta: None,
        }))
    }

    pub(super) fn handle_read_resource_directory(
        &self,
        params: ReadResourceDirectoryParams,
        extensions: &Extensions,
    ) -> Result<McpResponse> {
        let skills = match self.inner.skills.clone() {
            Some(skills) if skills.is_directory_read_enabled() => skills,
            _ => {
                return Err(Error::JsonRpc(JsonRpcError::method_not_found(
                    METHOD_RESOURCES_DIRECTORY_READ,
                )));
            }
        };
        let children = skills.directory_children(&params.uri).ok_or_else(|| {
            Error::invalid_params(format!("{} is not a directory resource", params.uri))
        })?;
        let filter_context = self.capability_filter_context(extensions, CapabilityOperation::List);
        let children: Vec<_> = children
            .into_iter()
            .filter(|child| {
                child.mime_type.as_deref() == Some(crate::skills::DIRECTORY_MIME_TYPE)
                    || self.skill_resource_visible(&filter_context, &child.uri)
            })
            .collect();
        let (resources, next_cursor) =
            paginate(children, params.cursor.as_deref(), self.inner.page_size)?;
        let (ttl_ms, cache_scope) = self.skills_cache_hints(&skills, self.inner.list_ttl_ms);
        Ok(McpResponse::ReadResourceDirectory(
            ReadResourceDirectoryResult {
                resources,
                next_cursor,
                ttl_ms: Some(ttl_ms),
                cache_scope: Some(cache_scope),
                meta: None,
            },
        ))
    }
}
