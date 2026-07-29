use tower_mcp::protocol::{
    Content, ElicitRequestParams, InputRequest, InputRequiredResult, ResourceContent,
};
use tower_mcp::{
    ElicitFormParams, ElicitFormSchema, ElicitMode, GetPromptResult, Prompt, PromptBuilder,
    PromptMessage, PromptRole, RequestOutcome,
};

use crate::tools::red_pixel_base64;

/// Build all conformance prompts.
pub fn build_prompts() -> Vec<Prompt> {
    vec![
        build_simple_prompt(),
        build_prompt_with_arguments(),
        build_prompt_with_embedded_resource(),
        build_prompt_with_image(),
        build_input_required_prompt(),
        build_exercise_conformance_prompt(),
    ]
}

fn build_input_required_prompt() -> Prompt {
    PromptBuilder::new("test_input_required_result_prompt")
        .description("SEP-2322 non-tool continuation fixture")
        .mrtr_handler(|ctx, _args| async move {
            if ctx
                .input_responses()
                .is_some_and(|responses| responses.contains_key("user_context"))
            {
                return Ok(RequestOutcome::Complete(GetPromptResult {
                    description: Some("Prompt using client-provided context".into()),
                    messages: vec![PromptMessage {
                        role: PromptRole::User,
                        content: Content::Text {
                            text: "Use the supplied test context".into(),
                            annotations: None,
                            meta: None,
                        },
                        meta: None,
                    }],
                    meta: None,
                }));
            }

            let request = InputRequest::Elicit(ElicitRequestParams::Form(ElicitFormParams {
                mode: Some(ElicitMode::Form),
                message: "What context should the prompt use?".into(),
                requested_schema: ElicitFormSchema::new().string_field(
                    "context",
                    Some("Context for the prompt"),
                    true,
                ),
                meta: None,
            }));
            Ok(RequestOutcome::input_required(
                InputRequiredResult::with_requests(
                    [("user_context".to_string(), request)]
                        .into_iter()
                        .collect(),
                ),
            ))
        })
        .build()
}

/// A prompt that guides an agent to exercise all conformance server features.
fn build_exercise_conformance_prompt() -> Prompt {
    PromptBuilder::new("exercise_conformance")
        .description("Exercise all MCP features to verify the server works correctly")
        .handler(|_args| async move {
            Ok(GetPromptResult {
                description: Some(
                    "Guide for exercising all MCP conformance server features".to_string(),
                ),
                messages: vec![PromptMessage {
                    role: PromptRole::User,
                    content: Content::Text {
                        text: r#"Please exercise all features of this MCP conformance server to verify it's working correctly.

## Tools to test:

1. **echo** - Echo back a message
2. **add** - Add two numbers together
3. **longRunningOperation** - Test progress notifications (watch for progress updates)
4. **sampleLLM** - Test sampling (makes a request back to you)
5. **getTinyImage** - Returns a small PNG image

## Resources to read:

1. **test://static/resource** - A static test resource
2. **test://static/resource/binary** - A binary resource

## Resource templates to try:

1. **test://dynamic/resource/{id}** - Try with different IDs like "hello" or "123"

## Prompts to get:

1. **test_simple_prompt** - A simple prompt with no arguments
2. **test_prompt_with_arguments** - Needs arg1 and arg2
3. **test_prompt_with_embedded_resource** - Embeds a resource
4. **test_prompt_with_image** - Contains an image

After testing each feature, summarize what worked and any issues found."#
                            .to_string(),
                        annotations: None,
                        meta: None,
                    },
                    meta: None,
                }],
                meta: None,
            })
        })
        .build()
}

fn build_simple_prompt() -> Prompt {
    PromptBuilder::new("test_simple_prompt")
        .description("A simple prompt with no arguments")
        .handler(|_args| async move {
            Ok(GetPromptResult {
                description: Some("A simple test prompt".to_string()),
                messages: vec![PromptMessage {
                    role: PromptRole::User,
                    content: Content::Text {
                        text: "This is a simple prompt message".to_string(),
                        annotations: None,
                        meta: None,
                    },
                    meta: None,
                }],
                meta: None,
            })
        })
        .build()
}

fn build_prompt_with_arguments() -> Prompt {
    PromptBuilder::new("test_prompt_with_arguments")
        .description("A prompt that accepts arguments")
        .required_arg("arg1", "First argument")
        .required_arg("arg2", "Second argument")
        .handler(|args| async move {
            let arg1 = args.get("arg1").cloned().unwrap_or_default();
            let arg2 = args.get("arg2").cloned().unwrap_or_default();
            Ok(GetPromptResult {
                description: Some("A prompt with arguments".to_string()),
                messages: vec![PromptMessage {
                    role: PromptRole::User,
                    content: Content::Text {
                        text: format!("arg1: {}, arg2: {}", arg1, arg2),
                        annotations: None,
                        meta: None,
                    },
                    meta: None,
                }],
                meta: None,
            })
        })
        .build()
}

fn build_prompt_with_embedded_resource() -> Prompt {
    PromptBuilder::new("test_prompt_with_embedded_resource")
        .description("A prompt that embeds a resource")
        .required_arg("resourceUri", "URI of the resource to embed")
        .handler(|args| async move {
            let resource_uri = args
                .get("resourceUri")
                .cloned()
                .unwrap_or_else(|| "test://embedded-resource".to_string());
            Ok(GetPromptResult {
                description: Some("A prompt with an embedded resource".to_string()),
                messages: vec![PromptMessage {
                    role: PromptRole::User,
                    content: Content::Resource {
                        resource: ResourceContent {
                            uri: resource_uri,
                            mime_type: Some("text/plain".to_string()),
                            text: Some("Embedded resource content".to_string()),
                            blob: None,
                            meta: None,
                        },
                        annotations: None,
                        meta: None,
                    },
                    meta: None,
                }],
                meta: None,
            })
        })
        .build()
}

fn build_prompt_with_image() -> Prompt {
    PromptBuilder::new("test_prompt_with_image")
        .description("A prompt that includes an image")
        .handler(|_args| async move {
            Ok(GetPromptResult {
                description: Some("A prompt with an image".to_string()),
                messages: vec![PromptMessage {
                    role: PromptRole::User,
                    content: Content::Image {
                        data: red_pixel_base64(),
                        mime_type: "image/png".to_string(),
                        annotations: None,
                        meta: None,
                    },
                    meta: None,
                }],
                meta: None,
            })
        })
        .build()
}
