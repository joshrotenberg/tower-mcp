//! A capability-scoped method answers `-32601` when the router does not
//! advertise that capability (#1654).
//!
//! Every test here goes through [`JsonRpcService`], so the 2026-07-28 cases
//! take the real per-request `_meta` path rather than hand-built extensions,
//! and the advertised capabilities are read from the `initialize` and
//! `server/discover` results a client would see.

use super::*;
use crate::jsonrpc::JsonRpcService;
use crate::prompt::PromptBuilder;
use crate::resource::ResourceBuilder;
use crate::tool::ToolBuilder;

/// The two lifecycles a request can arrive on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    /// 2025-11-25: `initialize`, then session-scoped requests.
    Session,
    /// 2026-07-28: no handshake, `_meta` on every request.
    #[cfg(feature = "stateless")]
    Stateless,
}

fn lifecycles() -> Vec<Lifecycle> {
    vec![
        Lifecycle::Session,
        #[cfg(feature = "stateless")]
        Lifecycle::Stateless,
    ]
}

/// Every capability-scoped method, the capability key it requires in the
/// advertised object, and parameters that name the probe fixtures below.
fn scoped_methods() -> Vec<(&'static str, &'static str, serde_json::Value)> {
    vec![
        ("tools/list", "tools", serde_json::json!({})),
        (
            "tools/call",
            "tools",
            serde_json::json!({"name": "probe", "arguments": {}}),
        ),
        ("resources/list", "resources", serde_json::json!({})),
        (
            "resources/templates/list",
            "resources",
            serde_json::json!({}),
        ),
        (
            "resources/read",
            "resources",
            serde_json::json!({"uri": "mem://probe"}),
        ),
        (
            "resources/subscribe",
            "resources",
            serde_json::json!({"uri": "mem://probe"}),
        ),
        (
            "resources/unsubscribe",
            "resources",
            serde_json::json!({"uri": "mem://probe"}),
        ),
        ("prompts/list", "prompts", serde_json::json!({})),
        (
            "prompts/get",
            "prompts",
            serde_json::json!({"name": "probe"}),
        ),
        (
            "completion/complete",
            "completions",
            serde_json::json!({
                "ref": {"type": "ref/prompt", "name": "probe"},
                "argument": {"name": "topic", "value": "a"}
            }),
        ),
    ]
}

/// Whether `method` exists on `lifecycle` at all. `resources/subscribe` and
/// `resources/unsubscribe` were removed from 2026-07-28 and are refused
/// before the router sees them, so they say nothing about the gate there.
fn method_exists(method: &str, lifecycle: Lifecycle) -> bool {
    lifecycle == Lifecycle::Session
        || !matches!(method, "resources/subscribe" | "resources/unsubscribe")
}

fn probe_tool() -> Tool {
    ToolBuilder::new("probe")
        .description("probe")
        .handler(|_: serde_json::Value| async move { Ok(CallToolResult::text("ok")) })
        .build()
}

fn probe_resource() -> Resource {
    ResourceBuilder::new("mem://probe").name("probe").text("hi")
}

fn probe_template() -> ResourceTemplate {
    crate::resource::ResourceTemplateBuilder::new("mem://items/{id}")
        .name("item")
        .handler(|uri: String, _vars| async move { Ok(ReadResourceResult::text(uri.clone(), uri)) })
}

fn probe_prompt() -> crate::prompt::Prompt {
    PromptBuilder::new("probe")
        .description("probe")
        .user_message("hi")
}

fn with_completions(router: McpRouter) -> McpRouter {
    router.completion_handler(|_params: CompleteParams| async move {
        Ok(CompleteResult::new(vec!["alpha".to_string()]))
    })
}

fn everything() -> McpRouter {
    with_completions(
        McpRouter::new()
            .tool(probe_tool())
            .resource(probe_resource())
            .resource_template(probe_template())
            .prompt(probe_prompt()),
    )
}

#[cfg(feature = "stateless")]
fn final_meta() -> serde_json::Value {
    serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION_2026_07_28,
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

/// A client on one lifecycle, talking to one router.
struct Peer {
    router: McpRouter,
    service: JsonRpcService<McpRouter>,
    lifecycle: Lifecycle,
    next_id: i64,
}

impl Peer {
    fn new(router: McpRouter, lifecycle: Lifecycle) -> Self {
        Self {
            service: JsonRpcService::new(router.clone()),
            router,
            lifecycle,
            next_id: 0,
        }
    }

    /// Run the lifecycle's opening exchange and return the capability object
    /// the server advertised in it.
    async fn open(&mut self) -> serde_json::Value {
        let result = match self.lifecycle {
            Lifecycle::Session => {
                let result = self
                    .request(
                        "initialize",
                        serde_json::json!({
                            "protocolVersion": "2025-11-25",
                            "capabilities": {},
                            "clientInfo": {"name": "probe", "version": "1.0"}
                        }),
                    )
                    .await
                    .expect("initialize succeeds");
                self.router
                    .handle_notification(McpNotification::Initialized);
                result
            }
            #[cfg(feature = "stateless")]
            Lifecycle::Stateless => self
                .request("server/discover", serde_json::json!({}))
                .await
                .expect("server/discover succeeds"),
        };
        result["capabilities"].clone()
    }

    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, JsonRpcError> {
        #[cfg(feature = "stateless")]
        let params = if self.lifecycle == Lifecycle::Stateless {
            let mut params = params;
            params["_meta"] = final_meta();
            params
        } else {
            params
        };
        self.next_id += 1;
        let request = JsonRpcRequest::new(self.next_id, method).with_params(params);
        match self.service.call_single(request).await.expect("infallible") {
            JsonRpcResponse::Result(result) => Ok(result.result),
            JsonRpcResponse::Error(error) => Err(error.error),
            other => panic!("unexpected response variant: {other:?}"),
        }
    }
}

fn assert_method_not_found(
    outcome: &std::result::Result<serde_json::Value, JsonRpcError>,
    method: &str,
    lifecycle: Lifecycle,
) {
    match outcome {
        Err(error) => {
            assert_eq!(
                error.code,
                crate::error::ErrorCode::MethodNotFound as i32,
                "{method} on {lifecycle:?} must be -32601, got {error:?}"
            );
            assert_eq!(error.message, format!("Method not found: {method}"));
        }
        Ok(result) => panic!("{method} on {lifecycle:?} must be -32601, got success {result}"),
    }
}

fn assert_routed(
    outcome: &std::result::Result<serde_json::Value, JsonRpcError>,
    method: &str,
    lifecycle: Lifecycle,
) {
    if let Err(error) = outcome {
        assert_ne!(
            error.code,
            crate::error::ErrorCode::MethodNotFound as i32,
            "{method} on {lifecycle:?} is advertised and must be routed, got {error:?}"
        );
    }
}

#[tokio::test]
async fn a_router_with_no_capabilities_refuses_every_scoped_method() {
    for lifecycle in lifecycles() {
        let mut peer = Peer::new(McpRouter::new(), lifecycle);
        let capabilities = peer.open().await;
        for key in ["tools", "resources", "prompts", "completions"] {
            assert!(
                capabilities.get(key).is_none(),
                "{key} must not be advertised on {lifecycle:?}: {capabilities}"
            );
        }
        for (method, _, params) in scoped_methods() {
            let outcome = peer.request(method, params).await;
            assert_method_not_found(&outcome, method, lifecycle);
        }
    }
}

#[tokio::test]
async fn a_tools_only_router_refuses_prompts_resources_and_completion() {
    for lifecycle in lifecycles() {
        let mut peer = Peer::new(McpRouter::new().tool(probe_tool()), lifecycle);
        let capabilities = peer.open().await;
        assert!(capabilities.get("tools").is_some(), "{capabilities}");

        let tools = peer
            .request("tools/list", serde_json::json!({}))
            .await
            .expect("tools/list is routed");
        assert_eq!(tools["tools"][0]["name"], "probe");
        let call = peer
            .request(
                "tools/call",
                serde_json::json!({"name": "probe", "arguments": {}}),
            )
            .await
            .expect("tools/call is routed");
        assert_eq!(call["content"][0]["text"], "ok");

        for (method, capability, params) in scoped_methods() {
            if capability == "tools" || !method_exists(method, lifecycle) {
                continue;
            }
            let outcome = peer.request(method, params).await;
            assert_method_not_found(&outcome, method, lifecycle);
        }
    }
}

/// With every capability advertised, each method behaves as it did before
/// the gate: populated lists, and successful call, read, get, and complete.
#[tokio::test]
async fn every_capability_present_keeps_existing_behavior() {
    for lifecycle in lifecycles() {
        let mut peer = Peer::new(everything(), lifecycle);
        let capabilities = peer.open().await;
        for key in ["tools", "resources", "prompts", "completions"] {
            assert!(capabilities.get(key).is_some(), "{key}: {capabilities}");
        }

        let ok = |outcome: std::result::Result<serde_json::Value, JsonRpcError>, method: &str| {
            outcome.unwrap_or_else(|error| panic!("{method} on {lifecycle:?} failed: {error:?}"))
        };
        let tools = ok(
            peer.request("tools/list", serde_json::json!({})).await,
            "tools/list",
        );
        assert_eq!(tools["tools"].as_array().unwrap().len(), 1);
        let resources = ok(
            peer.request("resources/list", serde_json::json!({})).await,
            "resources/list",
        );
        assert_eq!(resources["resources"][0]["uri"], "mem://probe");
        let templates = ok(
            peer.request("resources/templates/list", serde_json::json!({}))
                .await,
            "resources/templates/list",
        );
        assert_eq!(
            templates["resourceTemplates"][0]["uriTemplate"],
            "mem://items/{id}"
        );
        let read = ok(
            peer.request("resources/read", serde_json::json!({"uri": "mem://probe"}))
                .await,
            "resources/read",
        );
        assert_eq!(read["contents"][0]["text"], "hi");
        let prompts = ok(
            peer.request("prompts/list", serde_json::json!({})).await,
            "prompts/list",
        );
        assert_eq!(prompts["prompts"][0]["name"], "probe");
        ok(
            peer.request("prompts/get", serde_json::json!({"name": "probe"}))
                .await,
            "prompts/get",
        );
        let completion = ok(
            peer.request(
                "completion/complete",
                serde_json::json!({
                    "ref": {"type": "ref/prompt", "name": "probe"},
                    "argument": {"name": "topic", "value": "a"}
                }),
            )
            .await,
            "completion/complete",
        );
        assert_eq!(completion["completion"]["values"][0], "alpha");
        ok(
            peer.request(
                "tools/call",
                serde_json::json!({"name": "probe", "arguments": {}}),
            )
            .await,
            "tools/call",
        );
        if lifecycle == Lifecycle::Session {
            ok(
                peer.request(
                    "resources/subscribe",
                    serde_json::json!({"uri": "mem://probe"}),
                )
                .await,
                "resources/subscribe",
            );
            ok(
                peer.request(
                    "resources/unsubscribe",
                    serde_json::json!({"uri": "mem://probe"}),
                )
                .await,
                "resources/unsubscribe",
            );
        }
    }
}

/// A named way to build a router for the matrix below.
type Configuration = (&'static str, Box<dyn Fn() -> McpRouter>);

/// Router configurations the matrix below runs, by name.
fn configurations() -> Vec<Configuration> {
    #[cfg_attr(
        not(any(feature = "dynamic-tools", feature = "skills")),
        allow(unused_mut)
    )]
    let mut configurations: Vec<Configuration> = vec![
        ("empty", Box::new(McpRouter::new)),
        ("tools", Box::new(|| McpRouter::new().tool(probe_tool()))),
        (
            "resources",
            Box::new(|| McpRouter::new().resource(probe_resource())),
        ),
        (
            "templates",
            Box::new(|| McpRouter::new().resource_template(probe_template())),
        ),
        (
            "prompts",
            Box::new(|| McpRouter::new().prompt(probe_prompt())),
        ),
        (
            "completions",
            Box::new(|| with_completions(McpRouter::new())),
        ),
        ("everything", Box::new(everything)),
        (
            "tools with a filter hiding every tool",
            Box::new(|| {
                McpRouter::new()
                    .tool(probe_tool())
                    .tool_filter(crate::filter::CapabilityFilter::new(|_, _: &Tool| false))
            }),
        ),
        (
            "merged tools and prompts",
            Box::new(|| {
                McpRouter::new()
                    .merge(McpRouter::new().tool(probe_tool()))
                    .merge(McpRouter::new().prompt(probe_prompt()))
            }),
        ),
        (
            "nested resources",
            Box::new(|| {
                McpRouter::new().nest("inner", McpRouter::new().resource(probe_resource()))
            }),
        ),
        (
            // Router-level configuration is not merged, so a completion
            // handler on a merged router advertises nothing and is not routed.
            "merged completion handler",
            Box::new(|| McpRouter::new().merge(with_completions(McpRouter::new()))),
        ),
    ];
    #[cfg(feature = "dynamic-tools")]
    configurations.extend([
        (
            "empty dynamic tools",
            Box::new(|| McpRouter::new().with_dynamic_tools().0) as Box<dyn Fn() -> McpRouter>,
        ),
        (
            "empty dynamic prompts",
            Box::new(|| McpRouter::new().with_dynamic_prompts().0),
        ),
        (
            "empty dynamic resources",
            Box::new(|| McpRouter::new().with_dynamic_resources().0),
        ),
        (
            "empty dynamic resource templates",
            Box::new(|| McpRouter::new().with_dynamic_resource_templates().0),
        ),
        (
            "prompt initializer without a registry",
            Box::new(|| McpRouter::new().dynamic_prompt_initializer(|| Ok(()))),
        ),
    ]);
    #[cfg(feature = "skills")]
    configurations.push(("skills", Box::new(|| McpRouter::new().skills(skill_set()))));
    configurations
}

#[cfg(feature = "skills")]
fn skill_set() -> crate::skills::SkillSet {
    crate::skills::SkillSet::new()
        .skill(
            crate::skills::SkillBuilder::new("probe")
                .skill_md("---\nname: probe\ndescription: A probe skill\n---\nBody\n"),
        )
        .expect("valid skill")
}

/// For every configuration and lifecycle, a scoped method is refused with
/// `-32601` exactly when its capability is missing from what the router
/// advertised on that lifecycle. This is the property the shared predicate
/// guarantees.
#[tokio::test]
async fn the_gate_matches_the_advertised_capabilities() {
    for (name, build) in configurations() {
        for lifecycle in lifecycles() {
            let router = build();
            for (capability, scoped) in [
                ("tools", ScopedCapability::Tools),
                ("resources", ScopedCapability::Resources),
                ("prompts", ScopedCapability::Prompts),
                ("completions", ScopedCapability::Completions),
            ] {
                let version = match lifecycle {
                    Lifecycle::Session => "2025-11-25",
                    #[cfg(feature = "stateless")]
                    Lifecycle::Stateless => PROTOCOL_VERSION_2026_07_28,
                };
                let object = serde_json::to_value(router.capabilities_for_protocol(Some(version)))
                    .expect("capabilities serialize");
                assert_eq!(
                    object.get(capability).is_some(),
                    router.advertises(scoped),
                    "{name}: {capability} advertisement disagrees with the predicate on {lifecycle:?}"
                );
            }

            let mut peer = Peer::new(router, lifecycle);
            let capabilities = peer.open().await;
            for (method, capability, params) in scoped_methods() {
                if !method_exists(method, lifecycle) {
                    continue;
                }
                let outcome = peer.request(method, params).await;
                if capabilities.get(capability).is_some() {
                    assert_routed(&outcome, method, lifecycle);
                } else {
                    assert_method_not_found(&outcome, method, lifecycle);
                }
            }
        }
    }
}

/// A configured dynamic registry advertises its capability while empty, so
/// entries registered after the client connected are reachable without a
/// renegotiation the protocol does not have.
#[cfg(feature = "dynamic-tools")]
#[tokio::test]
async fn an_empty_dynamic_registry_advertises_and_routes() {
    for lifecycle in lifecycles() {
        let (router, tools) = McpRouter::new().with_dynamic_tools();
        let (router, prompts) = router.with_dynamic_prompts();
        let (router, resources) = router.with_dynamic_resources();
        let mut peer = Peer::new(router, lifecycle);
        let capabilities = peer.open().await;
        for key in ["tools", "resources", "prompts"] {
            assert!(capabilities.get(key).is_some(), "{key}: {capabilities}");
        }

        let empty = peer
            .request("tools/list", serde_json::json!({}))
            .await
            .expect("tools/list is routed while empty");
        assert_eq!(empty["tools"], serde_json::json!([]));
        let empty = peer
            .request("prompts/list", serde_json::json!({}))
            .await
            .expect("prompts/list is routed while empty");
        assert_eq!(empty["prompts"], serde_json::json!([]));
        let empty = peer
            .request("resources/list", serde_json::json!({}))
            .await
            .expect("resources/list is routed while empty");
        assert_eq!(empty["resources"], serde_json::json!([]));

        tools.register(probe_tool());
        prompts.register(probe_prompt());
        resources.register(probe_resource());

        let listed = peer
            .request("tools/list", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(listed["tools"][0]["name"], "probe");
        let listed = peer
            .request("prompts/list", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(listed["prompts"][0]["name"], "probe");
        let listed = peer
            .request("resources/list", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(listed["resources"][0]["uri"], "mem://probe");

        // Completions is not registry-backed and stays unadvertised.
        let outcome = peer
            .request(
                "completion/complete",
                serde_json::json!({
                    "ref": {"type": "ref/prompt", "name": "probe"},
                    "argument": {"name": "topic", "value": "a"}
                }),
            )
            .await;
        assert_method_not_found(&outcome, "completion/complete", lifecycle);
    }
}

/// A lazy initializer fills a dynamic registry; on its own it advertises
/// nothing, and the gate refuses `prompts/list` before running it.
#[cfg(feature = "dynamic-tools")]
#[tokio::test]
async fn a_prompt_initializer_alone_advertises_nothing() {
    use std::sync::atomic::AtomicUsize;

    for lifecycle in lifecycles() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = runs.clone();
        let router = McpRouter::new().dynamic_prompt_initializer(move || {
            counter.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        });
        let mut peer = Peer::new(router, lifecycle);
        let capabilities = peer.open().await;
        assert!(capabilities.get("prompts").is_none(), "{capabilities}");
        let outcome = peer.request("prompts/list", serde_json::json!({})).await;
        assert_method_not_found(&outcome, "prompts/list", lifecycle);
        assert_eq!(runs.load(AtomicOrdering::SeqCst), 0);

        // Paired with the registry it fills, the initializer runs as before.
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = runs.clone();
        let (router, registry) = McpRouter::new().with_dynamic_prompts();
        let router = router.dynamic_prompt_initializer(move || {
            if counter.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                registry.register(probe_prompt());
            }
            Ok(())
        });
        let mut peer = Peer::new(router, lifecycle);
        peer.open().await;
        let listed = peer
            .request("prompts/list", serde_json::json!({}))
            .await
            .expect("prompts/list is routed");
        assert_eq!(listed["prompts"][0]["name"], "probe");
        assert_eq!(runs.load(AtomicOrdering::SeqCst), 1);
    }
}

/// A skills catalog registers its files as resources and advertises
/// `resources` (SEP-2640), so the resource methods stay routed while the
/// capabilities it does not provide are refused.
#[cfg(feature = "skills")]
#[tokio::test]
async fn a_skills_catalog_advertises_and_routes_resources() {
    for lifecycle in lifecycles() {
        let mut peer = Peer::new(McpRouter::new().skills(skill_set()), lifecycle);
        let capabilities = peer.open().await;
        assert!(capabilities.get("resources").is_some(), "{capabilities}");

        let listed = peer
            .request("resources/list", serde_json::json!({}))
            .await
            .expect("resources/list is routed");
        assert_eq!(
            listed["resources"][0]["uri"], "skill://probe/SKILL.md",
            "{listed}"
        );
        peer.request(
            "resources/read",
            serde_json::json!({"uri": "skill://probe/SKILL.md"}),
        )
        .await
        .expect("resources/read is routed");
        peer.request("resources/templates/list", serde_json::json!({}))
            .await
            .expect("resources/templates/list is routed");

        for method in ["tools/list", "prompts/list"] {
            let outcome = peer.request(method, serde_json::json!({})).await;
            assert_method_not_found(&outcome, method, lifecycle);
        }
    }
}

/// `resource_subscriptions(false)` changes advertisement only, as its docs
/// promise: with `resources` advertised, `resources/subscribe` is routed.
#[tokio::test]
async fn declining_subscriptions_still_routes_subscribe() {
    let router = McpRouter::new()
        .resource(probe_resource())
        .resource_subscriptions(false);
    let mut peer = Peer::new(router, Lifecycle::Session);
    let capabilities = peer.open().await;
    assert_eq!(
        capabilities["resources"]["subscribe"],
        serde_json::json!(false)
    );
    peer.request(
        "resources/subscribe",
        serde_json::json!({"uri": "mem://probe"}),
    )
    .await
    .expect("resources/subscribe is routed");
}

/// Lifecycle methods are not capability-scoped, and `logging/setLevel` is
/// deliberately left ungated (see `ScopedCapability::required_by`).
#[tokio::test]
async fn unscoped_methods_are_unaffected() {
    let mut peer = Peer::new(McpRouter::new(), Lifecycle::Session);
    let capabilities = peer.open().await;
    assert!(capabilities.get("logging").is_none(), "{capabilities}");
    peer.request("ping", serde_json::json!({}))
        .await
        .expect("ping is routed");
    peer.request("logging/setLevel", serde_json::json!({"level": "info"}))
        .await
        .expect("logging/setLevel is routed");

    #[cfg(feature = "stateless")]
    {
        let mut peer = Peer::new(McpRouter::new(), Lifecycle::Stateless);
        peer.open().await;
        peer.request("server/discover", serde_json::json!({}))
            .await
            .expect("server/discover is routed");
    }
}
