//! Building an [`McpRouter`](super::McpRouter): registration and configuration.
//!
//! Everything here answers "what does this server offer" and is called before
//! the router ever sees a request. The dispatch half stays in the parent
//! module, next to the `Service` impl it serves (#1256).
//!
//! These are a second `impl McpRouter` block rather than a separate type, so
//! no call site changes and no public path moves.

use super::*;

impl McpRouter {
    /// Whether to advertise `resources.subscribe` when resources exist.
    ///
    /// Defaults to `true`, which is what this router has always advertised as
    /// soon as any resource or template is registered. Pass `false` for a
    /// server that exposes read-only resources and no update stream, so it
    /// does not promise a subscription it will not honour (#1261).
    ///
    /// This affects advertisement only. `resources/subscribe` continues to be
    /// routed either way, so a client that ignores the capability and calls it
    /// anyway behaves as before.
    ///
    /// The 2026-07-28 revision has no `resources/subscribe` method at all, so
    /// the capability is never advertised on that lifecycle regardless of this
    /// setting.
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ResourceBuilder};
    ///
    /// let router = McpRouter::new()
    ///     .server_info("read-only", "1.0.0")
    ///     .resource(ResourceBuilder::new("mem://one").name("one").text("hi"))
    ///     .resource_subscriptions(false);
    /// ```
    pub fn resource_subscriptions(mut self, advertise: bool) -> Self {
        Arc::make_mut(&mut self.inner).advertise_resource_subscriptions = advertise;
        self
    }

    /// Whether to advertise `tools.listChanged`.
    ///
    /// Defaults to whether a notification channel is attached to this
    /// router, which is what it has always advertised: choosing a transport
    /// that installs the channel (for example `StdioTransport::new`)
    /// promised `tools/list_changed` traffic even for a server that never
    /// sends it. Pass `true` or `false` to declare the flag independently of
    /// that channel (#1338).
    ///
    /// This affects advertisement only. Notifications are still routed
    /// through the channel exactly as before; this only changes what
    /// `initialize` reports.
    ///
    /// An explicit call here always wins over the notification-channel
    /// default, including under `StdioTransport::without_server_notifications`
    /// (#1257), which leaves the channel unattached. That method is the
    /// all-or-nothing switch; this builder is the per-flag refinement
    /// underneath it, so setting `tools_list_changed(true)` still advertises
    /// the flag even though the transport will never emit it.
    ///
    /// ```rust
    /// use tower_mcp::McpRouter;
    ///
    /// let router = McpRouter::new()
    ///     .server_info("my-server", "1.0.0")
    ///     .tools_list_changed(true);
    /// ```
    pub fn tools_list_changed(mut self, advertise: bool) -> Self {
        Arc::make_mut(&mut self.inner).advertise_tools_list_changed = Some(advertise);
        self
    }

    /// Whether to advertise `prompts.listChanged`.
    ///
    /// Defaults to whether a notification channel is attached to this
    /// router, which is what it has always advertised: choosing a transport
    /// that installs the channel (for example `StdioTransport::new`)
    /// promised `prompts/list_changed` traffic even for a server that never
    /// sends it. Pass `true` or `false` to declare the flag independently of
    /// that channel (#1338).
    ///
    /// This affects advertisement only. Notifications are still routed
    /// through the channel exactly as before; this only changes what
    /// `initialize` reports.
    ///
    /// An explicit call here always wins over the notification-channel
    /// default, including under `StdioTransport::without_server_notifications`
    /// (#1257), which leaves the channel unattached. That method is the
    /// all-or-nothing switch; this builder is the per-flag refinement
    /// underneath it, so setting `prompts_list_changed(true)` still
    /// advertises the flag even though the transport will never emit it.
    ///
    /// ```rust
    /// use tower_mcp::McpRouter;
    ///
    /// let router = McpRouter::new()
    ///     .server_info("my-server", "1.0.0")
    ///     .prompts_list_changed(false);
    /// ```
    pub fn prompts_list_changed(mut self, advertise: bool) -> Self {
        Arc::make_mut(&mut self.inner).advertise_prompts_list_changed = Some(advertise);
        self
    }

    /// Whether to advertise `resources.listChanged`.
    ///
    /// Defaults to whether a notification channel is attached to this
    /// router, which is what it has always advertised: choosing a transport
    /// that installs the channel (for example `StdioTransport::new`)
    /// promised `resources/list_changed` traffic even for a server that
    /// never sends it. Pass `true` or `false` to declare the flag
    /// independently of that channel (#1338).
    ///
    /// This affects advertisement only. Notifications are still routed
    /// through the channel exactly as before; this only changes what
    /// `initialize` reports. It is independent of
    /// [`Self::resource_subscriptions`], which governs `resources.subscribe`
    /// rather than `resources.listChanged`.
    ///
    /// An explicit call here always wins over the notification-channel
    /// default, including under `StdioTransport::without_server_notifications`
    /// (#1257), which leaves the channel unattached. That method is the
    /// all-or-nothing switch; this builder is the per-flag refinement
    /// underneath it, so setting `resources_list_changed(true)` still
    /// advertises the flag even though the transport will never emit it.
    ///
    /// ```rust
    /// use tower_mcp::McpRouter;
    ///
    /// let router = McpRouter::new()
    ///     .server_info("my-server", "1.0.0")
    ///     .resources_list_changed(false);
    /// ```
    pub fn resources_list_changed(mut self, advertise: bool) -> Self {
        Arc::make_mut(&mut self.inner).advertise_resources_list_changed = Some(advertise);
        self
    }

    /// Whether to advertise the `logging` capability (MCP logging, i.e.
    /// `notifications/message`).
    ///
    /// Defaults to whether a notification channel is attached to this
    /// router, which is what it has always advertised: choosing a transport
    /// that installs the channel (for example `StdioTransport::new`)
    /// promised MCP logging even for a server that logs elsewhere, such as
    /// stderr or OTLP. Pass `true` or `false` to declare the flag
    /// independently of that channel (#1338).
    ///
    /// This affects advertisement only. [`McpRouter::log`] and its
    /// convenience methods still route through the channel exactly as
    /// before; this only changes what `initialize` reports.
    ///
    /// An explicit call here always wins over the notification-channel
    /// default, including under `StdioTransport::without_server_notifications`
    /// (#1257), which leaves the channel unattached. That method is the
    /// all-or-nothing switch; this builder is the per-flag refinement
    /// underneath it, so setting `mcp_logging(true)` still advertises the
    /// flag even though the transport will never emit it.
    ///
    /// ```rust
    /// use tower_mcp::McpRouter;
    ///
    /// let router = McpRouter::new()
    ///     .server_info("my-server", "1.0.0")
    ///     .mcp_logging(false);
    /// ```
    pub fn mcp_logging(mut self, advertise: bool) -> Self {
        Arc::make_mut(&mut self.inner).advertise_mcp_logging = Some(advertise);
        self
    }

    /// Set server info
    pub fn server_info(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        let inner = Arc::make_mut(&mut self.inner);
        inner.server_name = name.into();
        inner.server_version = version.into();
        self
    }

    /// Set the page size for list method pagination.
    ///
    /// When set, list methods (`tools/list`, `resources/list`, etc.) will return
    /// at most `page_size` items per response, with a `next_cursor` for fetching
    /// subsequent pages. When `None` (the default), all items are returned in a
    /// single response.
    pub fn page_size(mut self, size: usize) -> Self {
        Arc::make_mut(&mut self.inner).page_size = Some(size);
        self
    }

    /// Set a TTL hint on list responses (tools/list, resources/list, prompts/list).
    ///
    /// When set, the `ttlMs` field is included in list responses so clients can
    /// cache the list for up to this many milliseconds before re-fetching.
    /// Implements SEP-2549.
    pub fn list_ttl(mut self, ms: u64) -> Self {
        Arc::make_mut(&mut self.inner).list_ttl_ms = Some(ms);
        self
    }

    /// Set a default TTL hint on resources/read responses (SEP-2549).
    ///
    /// Applied only when the resource handler did not set its own `ttl_ms`
    /// on the [`ReadResourceResult`]. When any TTL is emitted without a
    /// configured [`cache_scope`](Self::cache_scope), the scope defaults to
    /// `private`.
    pub fn read_ttl(mut self, ms: u64) -> Self {
        Arc::make_mut(&mut self.inner).read_ttl_ms = Some(ms);
        self
    }

    /// Set the SEP-2549 cache scope emitted alongside TTL hints on list and
    /// resources/read responses.
    ///
    /// `CacheScope::Public` allows any client, gateway, or proxy to reuse
    /// the cached result across authorization contexts; `CacheScope::Private`
    /// restricts reuse to the same authorization context. When a TTL is
    /// emitted and no scope is configured, `private` is used as the
    /// conservative default.
    pub fn cache_scope(mut self, scope: CacheScope) -> Self {
        Arc::make_mut(&mut self.inner).cache_scope = Some(scope);
        self
    }

    /// Mark the logging capability as deprecated in the server's initialize result.
    ///
    /// When set, the `deprecated` object is included in the `logging` capability
    /// in the `initialize` response, signalling to clients that logging notifications
    /// are being phased out. Implements SEP-2577.
    pub fn logging_deprecated(mut self, info: tower_mcp_types::protocol::DeprecationInfo) -> Self {
        Arc::make_mut(&mut self.inner).logging_deprecated = Some(info);
        self
    }

    /// Set instructions for LLMs describing how to use this server
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.inner).instructions = Some(instructions.into());
        self
    }

    /// Convert a panicking tool handler into an error result instead of
    /// letting it unwind out of the service.
    ///
    /// Without this, a panic in one handler ends the whole server over stdio
    /// and kills the connection task over HTTP. A bug in one tool should fail
    /// that call, not disconnect every client on the process, which is what
    /// makes this worth having on a long-running shared server.
    ///
    /// Off by default, deliberately. A panic is an invariant violation, and
    /// converting one into a tidy error result hides a bug that the author
    /// probably wants to see. Opting in is a statement that availability
    /// matters more than failing fast, which is true for a shared server and
    /// often false for a local one.
    ///
    /// For ordinary and replayed handlers, the caught panic becomes a
    /// `CallToolResult` with `is_error: true` carrying the panic message. A
    /// live Task handler instead reaches `failed` with the same detailed
    /// message. Both are logged at error level with the tool name so the panic
    /// is not silently swallowed.
    ///
    /// A panic that unwinds is caught; one that aborts the process (a
    /// double panic, or `panic = "abort"`) cannot be, by construction.
    pub fn catch_panics(mut self) -> Self {
        Arc::make_mut(&mut self.inner).panic_policy = Some(PanicPolicy::detailed());
        self
    }

    /// Convert a panicking tool handler into an error result using an
    /// application-selected disclosure policy.
    ///
    /// [`PanicPolicy::redacted`] returns fixed client text and omits both the
    /// tool name and panic payload from Tower's tracing event by default.
    /// Unlike [`McpRouter::catch_panics`], a custom policy never includes the
    /// panic payload in the client response.
    ///
    /// The policy applies to ordinary calls and both replayed and live Task
    /// handlers registered on this router. Router-level configuration is not
    /// imported when another router is merged or nested, so the receiving
    /// router's policy governs the combined catalog.
    ///
    /// Rust's process-global panic hook runs before the unwind is caught, so
    /// this controls Tower's client response and tracing event only. It does
    /// not suppress application or default panic-hook output.
    ///
    /// A panic that aborts the process (a double panic, or
    /// `panic = "abort"`) cannot be caught.
    pub fn catch_panics_with(mut self, policy: PanicPolicy) -> Self {
        Arc::make_mut(&mut self.inner).panic_policy = Some(policy);
        self
    }

    /// Auto-generate instructions from registered tool, resource, and prompt descriptions.
    ///
    /// The instructions are generated lazily at initialization time, so this can be
    /// called at any point in the builder chain regardless of when tools, resources,
    /// and prompts are registered.
    ///
    /// If both `instructions()` and `auto_instructions()` are set, the auto-generated
    /// instructions take precedence.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ToolBuilder, CallToolResult};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct QueryInput { sql: String }
    ///
    /// let query_tool = ToolBuilder::new("query")
    ///     .description("Execute a read-only SQL query")
    ///     .read_only()
    ///     .handler(|input: QueryInput| async move {
    ///         Ok(CallToolResult::text("result"))
    ///     })
    ///     .build();
    ///
    /// let router = McpRouter::new()
    ///     .auto_instructions()
    ///     .tool(query_tool);
    /// ```
    pub fn auto_instructions(mut self) -> Self {
        Arc::make_mut(&mut self.inner).auto_instructions = Some(AutoInstructionsConfig {
            prefix: None,
            suffix: None,
        });
        self
    }

    /// Auto-generate instructions with custom prefix and/or suffix text.
    ///
    /// The prefix is prepended and suffix appended to the generated instructions.
    /// See [`auto_instructions`](Self::auto_instructions) for details.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::McpRouter;
    ///
    /// let router = McpRouter::new()
    ///     .auto_instructions_with(
    ///         Some("This server provides database tools."),
    ///         Some("Use 'query' for read operations and 'insert' for writes."),
    ///     );
    /// ```
    pub fn auto_instructions_with(
        mut self,
        prefix: Option<impl Into<String>>,
        suffix: Option<impl Into<String>>,
    ) -> Self {
        Arc::make_mut(&mut self.inner).auto_instructions = Some(AutoInstructionsConfig {
            prefix: prefix.map(Into::into),
            suffix: suffix.map(Into::into),
        });
        self
    }

    /// Set a human-readable title for the server
    pub fn server_title(mut self, title: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.inner).server_title = Some(title.into());
        self
    }

    /// Set the server description
    pub fn server_description(mut self, description: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.inner).server_description = Some(description.into());
        self
    }

    /// Set icons for the server
    pub fn server_icons(mut self, icons: Vec<ToolIcon>) -> Self {
        Arc::make_mut(&mut self.inner).server_icons = Some(icons);
        self
    }

    /// Set the server's website URL
    pub fn server_website_url(mut self, url: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.inner).server_website_url = Some(url.into());
        self
    }

    /// Register a tool
    pub fn tool(mut self, tool: Tool) -> Self {
        Arc::make_mut(&mut self.inner)
            .tools
            .insert(tool.name.clone(), Arc::new(tool));
        self
    }

    /// Conditionally register a tool.
    ///
    /// Registers the tool only if `condition` is `true`. This keeps fluent
    /// builder chains intact when tools are conditionally enabled.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ToolBuilder, CallToolResult};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct Input { value: String }
    ///
    /// let enable_admin = false;
    ///
    /// let admin_tool = ToolBuilder::new("admin")
    ///     .description("Admin tool")
    ///     .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///     .build();
    ///
    /// let router = McpRouter::new()
    ///     .tool_if(enable_admin, admin_tool);
    /// ```
    pub fn tool_if(self, condition: bool, tool: Tool) -> Self {
        if condition { self.tool(tool) } else { self }
    }

    /// Register a resource
    pub fn resource(mut self, resource: Resource) -> Self {
        Arc::make_mut(&mut self.inner)
            .resources
            .insert(resource.uri.clone(), Arc::new(resource));
        self
    }

    /// Conditionally register a resource.
    ///
    /// Registers the resource only if `condition` is `true`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ResourceBuilder};
    ///
    /// let enable_config = false;
    ///
    /// let config = ResourceBuilder::new("config://system")
    ///     .name("config")
    ///     .text("secret=xxx");
    ///
    /// let router = McpRouter::new()
    ///     .resource_if(enable_config, config);
    /// ```
    pub fn resource_if(self, condition: bool, resource: Resource) -> Self {
        if condition {
            self.resource(resource)
        } else {
            self
        }
    }

    /// Register a resource template
    ///
    /// Resource templates allow dynamic resources to be matched by URI pattern.
    /// When a client requests a resource URI that doesn't match any static
    /// resource, the router tries to match it against registered templates.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ResourceTemplateBuilder};
    /// use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
    /// use std::collections::HashMap;
    ///
    /// let template = ResourceTemplateBuilder::new("file:///{path}")
    ///     .name("Project Files")
    ///     .handler(|uri: String, vars: HashMap<String, String>| async move {
    ///         let path = vars.get("path").unwrap_or(&String::new()).clone();
    ///         Ok(ReadResourceResult {
    ///             contents: vec![ResourceContent {
    ///                 uri,
    ///                 mime_type: Some("text/plain".to_string()),
    ///                 text: Some(format!("Contents of {}", path)),
    ///                 blob: None,
    ///                 meta: None,
    ///             }],
    ///             meta: None,
    ///             ..Default::default()
    ///         })
    ///     });
    ///
    /// let router = McpRouter::new()
    ///     .resource_template(template);
    /// ```
    pub fn resource_template(mut self, template: ResourceTemplate) -> Self {
        Arc::make_mut(&mut self.inner)
            .resource_templates
            .push(Arc::new(template));
        self
    }

    /// Register a prompt
    pub fn prompt(mut self, prompt: Prompt) -> Self {
        Arc::make_mut(&mut self.inner)
            .prompts
            .insert(prompt.name.clone(), Arc::new(prompt));
        self
    }

    /// Conditionally register a prompt.
    ///
    /// Registers the prompt only if `condition` is `true`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, PromptBuilder};
    ///
    /// let enable_debug = false;
    ///
    /// let debug_prompt = PromptBuilder::new("debug")
    ///     .description("Debug prompt")
    ///     .user_message("Debug mode enabled");
    ///
    /// let router = McpRouter::new()
    ///     .prompt_if(enable_debug, debug_prompt);
    /// ```
    pub fn prompt_if(self, condition: bool, prompt: Prompt) -> Self {
        if condition { self.prompt(prompt) } else { self }
    }

    /// Register multiple tools at once.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ToolBuilder, CallToolResult};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct Input { value: String }
    ///
    /// let tools = vec![
    ///     ToolBuilder::new("a")
    ///         .description("Tool A")
    ///         .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///         .build(),
    ///     ToolBuilder::new("b")
    ///         .description("Tool B")
    ///         .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///         .build(),
    /// ];
    ///
    /// let router = McpRouter::new().tools(tools);
    /// ```
    pub fn tools(self, tools: impl IntoIterator<Item = Tool>) -> Self {
        tools
            .into_iter()
            .fold(self, |router, tool| router.tool(tool))
    }

    /// Conditionally register multiple tools at once.
    ///
    /// Registers all tools only if `condition` is `true`.
    pub fn tools_if(self, condition: bool, tools: impl IntoIterator<Item = Tool>) -> Self {
        if condition { self.tools(tools) } else { self }
    }

    /// Register multiple resources at once.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ResourceBuilder};
    ///
    /// let resources = vec![
    ///     ResourceBuilder::new("file:///a.txt")
    ///         .name("File A")
    ///         .text("contents a"),
    ///     ResourceBuilder::new("file:///b.txt")
    ///         .name("File B")
    ///         .text("contents b"),
    /// ];
    ///
    /// let router = McpRouter::new().resources(resources);
    /// ```
    pub fn resources(self, resources: impl IntoIterator<Item = Resource>) -> Self {
        resources
            .into_iter()
            .fold(self, |router, resource| router.resource(resource))
    }

    /// Conditionally register multiple resources at once.
    ///
    /// Registers all resources only if `condition` is `true`.
    pub fn resources_if(
        self,
        condition: bool,
        resources: impl IntoIterator<Item = Resource>,
    ) -> Self {
        if condition {
            self.resources(resources)
        } else {
            self
        }
    }

    /// Register multiple prompts at once.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, PromptBuilder};
    ///
    /// let prompts = vec![
    ///     PromptBuilder::new("greet")
    ///         .description("Greet someone")
    ///         .user_message("Hello!"),
    ///     PromptBuilder::new("farewell")
    ///         .description("Say goodbye")
    ///         .user_message("Goodbye!"),
    /// ];
    ///
    /// let router = McpRouter::new().prompts(prompts);
    /// ```
    pub fn prompts(self, prompts: impl IntoIterator<Item = Prompt>) -> Self {
        prompts
            .into_iter()
            .fold(self, |router, prompt| router.prompt(prompt))
    }

    /// Conditionally register multiple prompts at once.
    ///
    /// Registers all prompts only if `condition` is `true`.
    pub fn prompts_if(self, condition: bool, prompts: impl IntoIterator<Item = Prompt>) -> Self {
        if condition {
            self.prompts(prompts)
        } else {
            self
        }
    }

    /// Merge another router's capabilities into this one.
    ///
    /// This combines all tools, resources, resource templates, and prompts from
    /// the other router into this router. Uses "last wins" semantics for conflicts,
    /// meaning if both routers have a tool/resource/prompt with the same name,
    /// the one from `other` will replace the one in `self`.
    ///
    /// Server info, instructions, filters, and other router-level configuration
    /// are NOT merged - only the root router's settings are used.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ToolBuilder, CallToolResult, ResourceBuilder};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct Input { value: String }
    ///
    /// // Create a router with database tools
    /// let db_tools = McpRouter::new()
    ///     .tool(
    ///         ToolBuilder::new("query")
    ///             .description("Query the database")
    ///             .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///             .build()
    ///     );
    ///
    /// // Create a router with API tools
    /// let api_tools = McpRouter::new()
    ///     .tool(
    ///         ToolBuilder::new("fetch")
    ///             .description("Fetch from API")
    ///             .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///             .build()
    ///     );
    ///
    /// // Merge them together
    /// let router = McpRouter::new()
    ///     .server_info("combined", "1.0")
    ///     .merge(db_tools)
    ///     .merge(api_tools);
    /// ```
    pub fn merge(mut self, other: McpRouter) -> Self {
        let inner = Arc::make_mut(&mut self.inner);
        let other_inner = other.inner;

        // Merge tools (last wins)
        for (name, tool) in &other_inner.tools {
            inner.tools.insert(name.clone(), tool.clone());
        }

        // Merge resources (last wins)
        for (uri, resource) in &other_inner.resources {
            inner.resources.insert(uri.clone(), resource.clone());
        }

        // Merge resource templates (append - no deduplication since templates
        // can have complex matching behavior)
        for template in &other_inner.resource_templates {
            inner.resource_templates.push(template.clone());
        }

        // Merge prompts (last wins)
        for (name, prompt) in &other_inner.prompts {
            inner.prompts.insert(name.clone(), prompt.clone());
        }

        // Merge protocol extension declarations (last wins).
        for (identifier, settings) in &other_inner.protocol_extensions {
            inner
                .protocol_extensions
                .insert(identifier.clone(), settings.clone());
        }

        self
    }

    /// Report the names both this router and `other` define.
    ///
    /// [`merge`](Self::merge) resolves a collision by letting the incoming
    /// router win, which is a reasonable default but leaves no trace that an
    /// implementation was dropped. A host that composes a router it does not
    /// own can call this first and fail at startup, which is the cheapest
    /// moment to catch the clash (#1232).
    ///
    /// Results are ordered by kind and then name, so they are stable enough
    /// to assert on and to print.
    ///
    /// Protocol extension declarations are deliberately excluded. Two routers
    /// both declaring the same extension is ordinary composition rather than
    /// a collision, since a declaration carries no implementation to lose.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ToolBuilder, CallToolResult};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct Input { value: String }
    ///
    /// fn router_with(name: &str) -> McpRouter {
    ///     McpRouter::new().tool(
    ///         ToolBuilder::new(name)
    ///             .description("example")
    ///             .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///             .build(),
    ///     )
    /// }
    ///
    /// let host = router_with("get_task");
    /// let library = router_with("get_task");
    /// let clashes = host.conflicts(&library);
    /// assert_eq!(clashes.len(), 1);
    /// assert_eq!(clashes[0].name, "get_task");
    /// ```
    pub fn conflicts(&self, other: &McpRouter) -> Vec<MergeConflict> {
        let mut found = Vec::new();

        for name in other.inner.tools.keys() {
            if self.inner.tools.contains_key(name) {
                found.push(MergeConflict::new(MergeConflictKind::Tool, name));
            }
        }
        for uri in other.inner.resources.keys() {
            if self.inner.resources.contains_key(uri) {
                found.push(MergeConflict::new(MergeConflictKind::Resource, uri));
            }
        }
        // Templates are stored as a list rather than a map because matching
        // is pattern-based, so identity here is the template string itself.
        for template in &other.inner.resource_templates {
            if self
                .inner
                .resource_templates
                .iter()
                .any(|existing| existing.uri_template == template.uri_template)
            {
                found.push(MergeConflict::new(
                    MergeConflictKind::ResourceTemplate,
                    &template.uri_template,
                ));
            }
        }
        for name in other.inner.prompts.keys() {
            if self.inner.prompts.contains_key(name) {
                found.push(MergeConflict::new(MergeConflictKind::Prompt, name));
            }
        }

        // `tools`, `resources`, and `prompts` are hash maps, so without this
        // the order would vary between runs.
        found.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
        found
    }

    /// Merge another router, failing if either defines a name the other does.
    ///
    /// This is [`merge`](Self::merge) with the collision reported instead of
    /// resolved. Use it when a silently dropped tool would surface later as a
    /// capability that behaves unexpectedly rather than as an error, which is
    /// the usual case when a host merges in a router from a library that
    /// cannot know what the host already registered.
    ///
    /// Callers who want the incoming router to win keep using
    /// [`merge`](Self::merge). To inspect without consuming either router,
    /// use [`conflicts`](Self::conflicts).
    ///
    /// # Errors
    ///
    /// Returns every conflicting name, not just the first, so a startup
    /// failure names all the work to be done.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ToolBuilder, CallToolResult};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct Input { value: String }
    ///
    /// fn router_with(name: &str) -> McpRouter {
    ///     McpRouter::new().tool(
    ///         ToolBuilder::new(name)
    ///             .description("example")
    ///             .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///             .build(),
    ///     )
    /// }
    ///
    /// // Distinct names merge.
    /// let combined = router_with("query").try_merge(router_with("fetch"));
    /// assert!(combined.is_ok());
    ///
    /// // A shared name is reported rather than dropped.
    /// let clash = router_with("get_task").try_merge(router_with("get_task"));
    /// let error = clash.unwrap_err();
    /// assert_eq!(error.conflicts().len(), 1);
    /// ```
    pub fn try_merge(self, other: McpRouter) -> std::result::Result<Self, MergeConflicts> {
        let conflicts = self.conflicts(&other);
        if conflicts.is_empty() {
            Ok(self.merge(other))
        } else {
            Err(MergeConflicts { conflicts })
        }
    }

    /// Nest another router's capabilities under a prefix.
    ///
    /// This is similar to `merge()`, but all tool names from the nested router
    /// are prefixed with the given string and a dot separator. For example,
    /// nesting with prefix "db" will turn a tool named "query" into "db.query".
    ///
    /// Resources, resource templates, and prompts are merged without modification
    /// since they use URIs rather than simple names for identification.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ToolBuilder, CallToolResult};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct Input { value: String }
    ///
    /// // Create a router with database tools
    /// let db_tools = McpRouter::new()
    ///     .tool(
    ///         ToolBuilder::new("query")
    ///             .description("Query the database")
    ///             .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///             .build()
    ///     )
    ///     .tool(
    ///         ToolBuilder::new("insert")
    ///             .description("Insert into database")
    ///             .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///             .build()
    ///     );
    ///
    /// // Nest under "db" prefix - tools become "db.query" and "db.insert"
    /// let router = McpRouter::new()
    ///     .server_info("combined", "1.0")
    ///     .nest("db", db_tools);
    /// ```
    pub fn nest(mut self, prefix: impl Into<String>, other: McpRouter) -> Self {
        let prefix = prefix.into();
        let inner = Arc::make_mut(&mut self.inner);
        let other_inner = other.inner;

        // Nest tools with prefix
        for tool in other_inner.tools.values() {
            let prefixed_tool = tool.with_name_prefix(&prefix);
            inner
                .tools
                .insert(prefixed_tool.name.clone(), Arc::new(prefixed_tool));
        }

        // Merge resources (no prefix - URIs are already namespaced)
        for (uri, resource) in &other_inner.resources {
            inner.resources.insert(uri.clone(), resource.clone());
        }

        // Merge resource templates (no prefix)
        for template in &other_inner.resource_templates {
            inner.resource_templates.push(template.clone());
        }

        // Merge prompts (no prefix - could be added in future if needed)
        for (name, prompt) in &other_inner.prompts {
            inner.prompts.insert(name.clone(), prompt.clone());
        }

        // Protocol extensions are server-wide declarations and are not
        // namespace-prefixed. Nested declarations use last-write-wins.
        for (identifier, settings) in &other_inner.protocol_extensions {
            inner
                .protocol_extensions
                .insert(identifier.clone(), settings.clone());
        }

        self
    }

    /// Register a completion handler for `completion/complete` requests.
    ///
    /// The handler receives `CompleteParams` containing the reference (prompt or resource)
    /// and the argument being completed, and should return completion suggestions.
    /// The referenced prompt, resource, or resource template must be registered, enabled,
    /// and visible to the request before this handler is invoked.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, CompleteResult};
    /// use tower_mcp::protocol::{CompleteParams, CompletionReference};
    ///
    /// let router = McpRouter::new()
    ///     .completion_handler(|params: CompleteParams| async move {
    ///         // Provide completions based on the reference and argument
    ///         match params.reference {
    ///             CompletionReference::Prompt { name } => {
    ///                 // Return prompt argument completions
    ///                 Ok(CompleteResult::new(vec!["option1".to_string(), "option2".to_string()]))
    ///             }
    ///             CompletionReference::Resource { uri } => {
    ///                 // Return resource URI completions
    ///                 Ok(CompleteResult::new(vec![]))
    ///             }
    ///             _ => Ok(CompleteResult::new(vec![])),
    ///         }
    ///     });
    /// ```
    pub fn completion_handler<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(CompleteParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<CompleteResult>> + Send + 'static,
    {
        Arc::make_mut(&mut self.inner).completion_handler =
            Some(Arc::new(move |_ctx, params| Box::pin(handler(params))));
        self
    }

    /// Register a request-aware completion handler for `completion/complete` requests.
    ///
    /// The [`RequestContext`] exposes router and per-request extensions, negotiated
    /// capabilities, progress, and cancellation. As with [`Self::completion_handler`],
    /// the referenced capability is resolved and authorized before dispatch.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{CompleteParams, CompleteResult, McpRouter, RequestContext};
    ///
    /// let router = McpRouter::new().completion_handler_with_context(
    ///     |ctx: RequestContext, params: CompleteParams| async move {
    ///         if ctx.is_cancelled() {
    ///             return Ok(CompleteResult::new(vec![]));
    ///         }
    ///         Ok(CompleteResult::new(vec![params.argument.value]))
    ///     },
    /// );
    /// ```
    pub fn completion_handler_with_context<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(RequestContext, CompleteParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<CompleteResult>> + Send + 'static,
    {
        Arc::make_mut(&mut self.inner).completion_handler =
            Some(Arc::new(move |ctx, params| Box::pin(handler(ctx, params))));
        self
    }

    /// Set a filter for tools based on session state.
    ///
    /// The filter determines which tools are visible to each session. Tools that
    /// don't pass the filter will not appear in `tools/list` responses and will
    /// return an error if called directly.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ToolBuilder, CallToolResult, CapabilityFilter, Tool, Filterable};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct Input { value: String }
    ///
    /// let public_tool = ToolBuilder::new("public")
    ///     .description("Available to everyone")
    ///     .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///     .build();
    ///
    /// let admin_tool = ToolBuilder::new("admin")
    ///     .description("Admin only")
    ///     .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///     .build();
    ///
    /// let router = McpRouter::new()
    ///     .tool(public_tool)
    ///     .tool(admin_tool)
    ///     .tool_filter(CapabilityFilter::new(|_session, tool: &Tool| {
    ///         // In real code, check session.extensions() for auth claims
    ///         tool.name() != "admin"
    ///     }));
    /// ```
    pub fn tool_filter(mut self, filter: ToolFilter) -> Self {
        Arc::make_mut(&mut self.inner).tool_filter = Some(filter);
        self
    }

    /// Set a filter for resources based on session state.
    ///
    /// The filter receives the current session state and each resource, returning
    /// `true` if the resource should be visible to this session. Resources that
    /// don't pass the filter will not appear in `resources/list` responses and will
    /// return an error if read directly. The same policy authorizes exact static
    /// resources for `resources/subscribe` and `resources/unsubscribe`; contextual
    /// filters receive a [`CapabilityOperation::Access`] whose target is the
    /// requested URI. Authorization runs before subscription membership is read or
    /// changed, so hidden resources cannot disclose whether a session subscribed.
    ///
    /// Resource templates require a separate
    /// [`resource_template_filter`](Self::resource_template_filter), because
    /// their policy must evaluate both a template definition and each concrete
    /// URI it resolves. If this resource filter is configured without a
    /// resource template filter, all templates fail closed: they are omitted
    /// from `resources/templates/list` and matching reads are denied with this
    /// filter's denial behavior.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ResourceBuilder, ReadResourceResult, CapabilityFilter, Resource, Filterable};
    ///
    /// let public_resource = ResourceBuilder::new("file:///public.txt")
    ///     .name("Public File")
    ///     .description("Available to everyone")
    ///     .text("public content");
    ///
    /// let secret_resource = ResourceBuilder::new("file:///secret.txt")
    ///     .name("Secret File")
    ///     .description("Admin only")
    ///     .text("secret content");
    ///
    /// let router = McpRouter::new()
    ///     .resource(public_resource)
    ///     .resource(secret_resource)
    ///     .resource_filter(CapabilityFilter::new(|_session, resource: &Resource| {
    ///         // In real code, check session.extensions() for auth claims
    ///         !resource.name().contains("Secret")
    ///     }));
    /// ```
    pub fn resource_filter(mut self, filter: ResourceFilter) -> Self {
        Arc::make_mut(&mut self.inner).resource_filter = Some(filter);
        self
    }

    /// Set a filter for resource template discovery and resolved reads.
    ///
    /// A contextual filter receives [`CapabilityOperation::List`] while a
    /// template definition is being listed, and an access operation whose
    /// target is the concrete requested URI before the matched handler runs.
    /// Denial stops at that matched template and never falls through to a later
    /// overlapping template.
    ///
    /// ```rust
    /// use tower_mcp::{
    ///     CapabilityFilter, McpRouter, ReadResourceResult, ResourceTemplate,
    ///     ResourceTemplateBuilder,
    /// };
    ///
    /// let template = ResourceTemplateBuilder::new("vault://{area}/{id}")
    ///     .name("vault")
    ///     .handler(|uri, _variables| async move {
    ///         Ok(ReadResourceResult::text(uri, "contents"))
    ///     });
    ///
    /// let router = McpRouter::new()
    ///     .resource_template(template)
    ///     .resource_template_filter(CapabilityFilter::new_with_context(
    ///         |context, _template: &ResourceTemplate| {
    ///             context.target().is_none_or(|uri| !uri.starts_with("vault://private/"))
    ///         },
    ///     ));
    /// ```
    pub fn resource_template_filter(mut self, filter: ResourceTemplateFilter) -> Self {
        Arc::make_mut(&mut self.inner).resource_template_filter = Some(filter);
        self
    }

    /// Set a filter for prompts based on session state.
    ///
    /// The filter receives the current session state and each prompt, returning
    /// `true` if the prompt should be visible to this session. Prompts that
    /// don't pass the filter will not appear in `prompts/list` responses and will
    /// return an error if accessed directly.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, PromptBuilder, CapabilityFilter, Prompt, Filterable};
    ///
    /// let public_prompt = PromptBuilder::new("greeting")
    ///     .description("A friendly greeting")
    ///     .user_message("Hello!");
    ///
    /// let admin_prompt = PromptBuilder::new("system_debug")
    ///     .description("Admin debugging prompt")
    ///     .user_message("Debug info");
    ///
    /// let router = McpRouter::new()
    ///     .prompt(public_prompt)
    ///     .prompt(admin_prompt)
    ///     .prompt_filter(CapabilityFilter::new(|_session, prompt: &Prompt| {
    ///         // In real code, check session.extensions() for auth claims
    ///         !prompt.name().contains("system")
    ///     }));
    /// ```
    pub fn prompt_filter(mut self, filter: PromptFilter) -> Self {
        Arc::make_mut(&mut self.inner).prompt_filter = Some(filter);
        self
    }

    /// Get access to the session state
    pub fn session(&self) -> &SessionState {
        &self.session
    }

    /// Disable a tool by name. Disabled tools are hidden from `tools/list`
    /// and return a method-not-found error from `tools/call`, but the tool
    /// definition stays attached to the router and can be flipped back on
    /// with [`enable_tool`](Self::enable_tool).
    ///
    /// State is shared across all clones produced by
    /// [`with_fresh_session`](Self::with_fresh_session), so flipping it once
    /// affects every connected session at the next request boundary. Call
    /// [`notify_tools_list_changed`](Self::notify_tools_list_changed) to nudge
    /// clients to re-fetch.
    pub fn disable_tool(&self, name: impl Into<String>) {
        let mut set = self.inner.disabled_tools.write().unwrap();
        set.insert(name.into());
    }

    /// Re-enable a previously disabled tool. No-op if the tool was not
    /// disabled.
    pub fn enable_tool(&self, name: &str) {
        let mut set = self.inner.disabled_tools.write().unwrap();
        set.remove(name);
    }

    /// Returns `true` if the named tool is currently enabled (i.e. not in
    /// the disabled set). Returns `true` even for unknown tool names; this
    /// only reports disable state, not registration.
    pub fn is_tool_enabled(&self, name: &str) -> bool {
        !self.inner.disabled_tools.read().unwrap().contains(name)
    }

    /// Disable a resource by concrete URI. Disabled resources are hidden from
    /// `resources/list` and return a not-found error from `resources/read`,
    /// `resources/subscribe`, and `resources/unsubscribe`, including when the URI
    /// would otherwise resolve through a template. Existing subscription membership
    /// remains inaccessible until the resource is re-enabled, or is discarded when
    /// the session ends. A disabled concrete URI does not hide the template
    /// definition or disable sibling URIs served by the same template.
    pub fn disable_resource(&self, uri: impl Into<String>) {
        let mut set = self.inner.disabled_resources.write().unwrap();
        set.insert(uri.into());
    }

    /// Re-enable a previously disabled resource.
    pub fn enable_resource(&self, uri: &str) {
        let mut set = self.inner.disabled_resources.write().unwrap();
        set.remove(uri);
    }

    /// Returns `true` if the resource at this URI is currently enabled.
    pub fn is_resource_enabled(&self, uri: &str) -> bool {
        !self.inner.disabled_resources.read().unwrap().contains(uri)
    }

    /// Disable a prompt by name. Disabled prompts are hidden from
    /// `prompts/list` and return a method-not-found error from `prompts/get`.
    pub fn disable_prompt(&self, name: impl Into<String>) {
        let mut set = self.inner.disabled_prompts.write().unwrap();
        set.insert(name.into());
    }

    /// Re-enable a previously disabled prompt.
    pub fn enable_prompt(&self, name: &str) {
        let mut set = self.inner.disabled_prompts.write().unwrap();
        set.remove(name);
    }

    /// Returns `true` if the named prompt is currently enabled.
    pub fn is_prompt_enabled(&self, name: &str) -> bool {
        !self.inner.disabled_prompts.read().unwrap().contains(name)
    }

    /// The server's identity, as configured via `.server_info()` and the
    /// related `.server_title()` / `.server_description()` / etc. builders.
    ///
    /// Shared by the `initialize` and `server/discover` handlers, and by the
    /// 2026-07-28 stateless HTTP dispatch (SEP-2575's "servers SHOULD
    /// identify themselves in each result's `_meta`") since that path calls
    /// in from outside this module and has no other way to read identity
    /// off a router wrapped behind arbitrary `.layer()` middleware.
    pub(crate) fn implementation(&self) -> Implementation {
        Implementation {
            name: self.inner.server_name.clone(),
            version: self.inner.server_version.clone(),
            title: self.inner.server_title.clone(),
            description: self.inner.server_description.clone(),
            icons: self.inner.server_icons.clone(),
            website_url: self.inner.server_website_url.clone(),
            meta: None,
        }
    }

    /// Return a snapshot of a registered tool's input schema.
    ///
    /// HTTP transport validation uses this before dispatch to enforce
    /// SEP-2243 `x-mcp-header` mappings. Static tools take precedence over
    /// dynamic tools, matching `tools/list` and `tools/call`.
    #[cfg(feature = "http")]
    pub(crate) fn tool_input_schema(&self, name: &str) -> Option<serde_json::Value> {
        if let Some(tool) = self.inner.tools.get(name) {
            return Some(tool.input_schema.clone());
        }
        #[cfg(feature = "dynamic-tools")]
        if let Some(tool) = self
            .inner
            .dynamic_tools
            .as_ref()
            .and_then(|tools| tools.get(name))
        {
            return Some(tool.input_schema.clone());
        }
        None
    }
}
