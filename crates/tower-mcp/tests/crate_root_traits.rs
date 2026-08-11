//! The trait-based way to define a tool, resource, or prompt must be usable
//! from the crate root.
//!
//! `McpTool`, `McpResource`, and `McpPrompt` are the documented alternative to
//! the builders, but each one's doc example named it through its own module
//! (`tower_mcp::tool::McpTool`). That path resolves whether or not `lib.rs`
//! re-exports the trait, so nothing compiled ever depended on
//! `tower_mcp::McpTool` and all three were missing from the crate root.
//!
//! Same shape as #1240 (`notification_channel`) and #1258 (`JsonRpcError`):
//! the internal path works, so the crate-root path goes unexercised and the
//! omission survives. A doc example cannot stand in for this test, because
//! doctests do not run in CI (#1275).
//!
//! Every import below is deliberately `tower_mcp::Item`. Rewriting any of
//! them to `tower_mcp::tool::Item` would defeat the point of the file.

use std::collections::HashMap;

use serde::Deserialize;
use tower_mcp::schemars::JsonSchema;
use tower_mcp::{
    Content, GetPromptResult, McpPrompt, McpResource, McpRouter, McpTool, PromptArgument,
    PromptMessage, PromptRole, ReadResourceResult, ResourceContent, Result, ToolAnnotations,
};

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

struct AddTool;

impl McpTool for AddTool {
    const NAME: &'static str = "add";
    const DESCRIPTION: &'static str = "Add two numbers";

    type Input = AddInput;
    type Output = i64;

    async fn call(&self, input: Self::Input) -> Result<Self::Output> {
        Ok(input.a + input.b)
    }

    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            read_only_hint: true,
            ..Default::default()
        })
    }
}

struct ConfigResource;

impl McpResource for ConfigResource {
    const URI: &'static str = "file:///config.json";
    const NAME: &'static str = "Configuration";
    const DESCRIPTION: Option<&'static str> = Some("Application configuration");
    const MIME_TYPE: Option<&'static str> = Some("application/json");

    async fn read(&self) -> Result<ReadResourceResult> {
        Ok(ReadResourceResult {
            contents: vec![ResourceContent {
                uri: Self::URI.to_string(),
                mime_type: Self::MIME_TYPE.map(|mime| mime.to_string()),
                text: Some("{}".to_string()),
                blob: None,
                meta: None,
            }],
            meta: None,
            ..Default::default()
        })
    }
}

struct GreetPrompt;

impl McpPrompt for GreetPrompt {
    const NAME: &'static str = "greet";
    const DESCRIPTION: &'static str = "Greet someone";

    fn arguments(&self) -> Vec<PromptArgument> {
        vec![PromptArgument {
            name: "name".to_string(),
            description: Some("Who to greet".to_string()),
            required: true,
        }]
    }

    async fn get(&self, arguments: HashMap<String, String>) -> Result<GetPromptResult> {
        let name = arguments.get("name").map_or("world", String::as_str);
        Ok(GetPromptResult {
            description: Some(Self::DESCRIPTION.to_string()),
            messages: vec![PromptMessage {
                role: PromptRole::User,
                content: Content::Text {
                    text: format!("Hello, {name}"),
                    annotations: None,
                    meta: None,
                },
                meta: None,
            }],
            meta: None,
        })
    }
}

#[tokio::test]
async fn a_trait_defined_tool_carries_its_metadata_and_runs() {
    let tool = AddTool.into_tool();

    let definition = tool.definition();
    assert_eq!(definition.name, "add");
    assert_eq!(definition.description.as_deref(), Some("Add two numbers"));
    assert!(
        definition
            .annotations
            .as_ref()
            .is_some_and(|annotations| annotations.read_only_hint),
        "the trait's annotations() override should reach the definition"
    );

    let result = tool.call(serde_json::json!({"a": 2, "b": 3})).await;
    assert!(!result.is_error);
    assert_eq!(
        result.structured_content,
        Some(serde_json::json!(5)),
        "the trait's Output should reach the result unchanged"
    );
}

#[tokio::test]
async fn a_trait_defined_resource_carries_its_metadata_and_reads() {
    let resource = ConfigResource.into_resource();

    let definition = resource.definition();
    assert_eq!(definition.uri, "file:///config.json");
    assert_eq!(definition.name, "Configuration");
    assert_eq!(definition.mime_type.as_deref(), Some("application/json"));

    let result = resource.read().await;
    assert_eq!(result.contents.len(), 1);
    assert_eq!(result.contents[0].text.as_deref(), Some("{}"));
}

#[tokio::test]
async fn a_trait_defined_prompt_carries_its_arguments_and_expands() {
    let prompt = GreetPrompt.into_prompt();

    let definition = prompt.definition();
    assert_eq!(definition.name, "greet");
    assert_eq!(definition.arguments.len(), 1);
    assert_eq!(definition.arguments[0].name, "name");

    let result = prompt
        .get(HashMap::from([("name".to_string(), "Ada".to_string())]))
        .await
        .expect("prompt expansion");
    assert_eq!(result.messages.len(), 1);
    let Content::Text { text, .. } = &result.messages[0].content else {
        panic!("expected a text message");
    };
    assert_eq!(text, "Hello, Ada");
}

#[test]
fn all_three_register_on_a_router() {
    // That the router accepts all three is itself the assertion: `tool`,
    // `resource`, and `prompt` take the concrete types the traits produce, so
    // this stops compiling if `into_tool` and friends stop lining up with the
    // registration API. `tool_annotations_map` is the one registry read the
    // router exposes, and it confirms the tool landed under its own name.
    let router = McpRouter::new()
        .tool(AddTool.into_tool())
        .resource(ConfigResource.into_resource())
        .prompt(GreetPrompt.into_prompt());

    let annotations = router.tool_annotations_map();
    assert!(annotations.get("add").is_some(), "the tool should register");
    assert!(annotations.is_read_only("add"));
}
