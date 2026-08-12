//! Required prompt arguments are enforced centrally (#1281).
//!
//! `PromptBuilder` lets a server declare an argument required, and that flows
//! into `prompts/list`, so clients are told it is required. The router never
//! checked, which left every handler re-implementing the same validation or
//! reading a missing argument as absent and producing something wrong.
//!
//! The spec requires a missing required argument to produce `-32602`, so the
//! check lives in the router ahead of dispatch: one place, shared by the
//! layered and unlayered paths alike.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use tower_mcp::protocol::{Content, GetPromptResult, PromptMessage, PromptRole};
use tower_mcp::{McpRouter, PromptBuilder};

fn message(text: &str) -> GetPromptResult {
    GetPromptResult {
        description: None,
        messages: vec![PromptMessage {
            role: PromptRole::User,
            content: Content::Text {
                text: text.to_string(),
                annotations: None,
                meta: None,
            },
            meta: None,
        }],
        meta: None,
    }
}

/// A prompt with one required and one optional argument, optionally layered.
fn router(layered: bool) -> McpRouter {
    let build = || {
        PromptBuilder::new("greet")
            .description("Greets someone")
            .required_arg("name", "Who to greet")
            .optional_arg("title", "An honorific")
    };
    let prompt = if layered {
        build()
            .handler(|args: HashMap<String, String>| async move {
                Ok(message(&format!(
                    "hello {}",
                    args.get("name").cloned().unwrap_or_default()
                )))
            })
            .layer(tower::timeout::TimeoutLayer::new(Duration::from_secs(30)))
    } else {
        build()
            .handler(|args: HashMap<String, String>| async move {
                Ok(message(&format!(
                    "hello {}",
                    args.get("name").cloned().unwrap_or_default()
                )))
            })
            .build()
    };
    McpRouter::new()
        .server_info("prompts", "1.0.0")
        .prompt(prompt)
}

async fn get(router: &McpRouter, arguments: serde_json::Value) -> serde_json::Value {
    let client = tower_mcp::client::McpClient::connect(tower_mcp::client::ChannelTransport::new(
        router.clone(),
    ))
    .await
    .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");
    let arguments: HashMap<String, String> = serde_json::from_value(arguments).expect("arguments");
    match client.get_prompt("greet", Some(arguments)).await {
        Ok(result) => json!({ "ok": serde_json::to_value(result).unwrap() }),
        Err(error) => json!({ "err": error.to_string() }),
    }
}

/// The headline: omitting a declared-required argument is `-32602`, and the
/// handler is never reached.
#[tokio::test]
async fn a_missing_required_argument_is_rejected() {
    for layered in [false, true] {
        let outcome = get(&router(layered), json!({})).await;
        let error = outcome["err"]
            .as_str()
            .unwrap_or_else(|| panic!("layered={layered}: expected an error, got {outcome}"));
        assert!(
            error.contains("-32602") || error.to_lowercase().contains("invalid params"),
            "layered={layered}: expected invalid params, got {error}"
        );
        assert!(
            error.contains("name"),
            "layered={layered}: the error must name the missing argument: {error}"
        );
    }
}

/// Layering must not change the answer. This is the same assertion as above,
/// stated as the property #1280 is about: middleware composes without altering
/// the semantics of what it wraps.
#[tokio::test]
async fn layered_and_unlayered_prompts_agree() {
    let unlayered = get(&router(false), json!({})).await;
    let layered = get(&router(true), json!({})).await;
    assert!(unlayered.get("err").is_some() && layered.get("err").is_some());

    let unlayered_ok = get(&router(false), json!({"name": "ada"})).await;
    let layered_ok = get(&router(true), json!({"name": "ada"})).await;
    assert_eq!(
        unlayered_ok["ok"]["messages"][0]["content"]["text"],
        layered_ok["ok"]["messages"][0]["content"]["text"],
    );
}

/// An empty string is a value, not an omission. A handler may legitimately
/// want one, and the router has no business deciding otherwise.
#[tokio::test]
async fn an_empty_string_counts_as_present() {
    let outcome = get(&router(false), json!({"name": ""})).await;
    assert!(
        outcome.get("ok").is_some(),
        "an empty value is supplied, not missing: {outcome}"
    );
}

/// Optional arguments may be omitted, and unknown ones are not rejected: the
/// protocol does not say prompt arguments are a closed set.
#[tokio::test]
async fn optional_omissions_and_unknown_extras_are_accepted() {
    let omitted = get(&router(false), json!({"name": "ada"})).await;
    assert!(
        omitted.get("ok").is_some(),
        "optional may be omitted: {omitted}"
    );

    let extra = get(&router(false), json!({"name": "ada", "unexpected": "x"})).await;
    assert!(
        extra.get("ok").is_some(),
        "extra keys are not rejected: {extra}"
    );
}
