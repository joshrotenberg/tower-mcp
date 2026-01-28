use tower_mcp::protocol::{Content, ResourceContent};
use tower_mcp::{GetPromptResult, Prompt, PromptBuilder, PromptMessage, PromptRole};

use crate::tools::red_pixel_base64;

/// Build all conformance prompts.
pub fn build_prompts() -> Vec<Prompt> {
    vec![
        build_simple_prompt(),
        build_prompt_with_arguments(),
        build_prompt_with_embedded_resource(),
        build_prompt_with_image(),
    ]
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
                    },
                }],
            })
        })
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
                    },
                }],
            })
        })
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
                        },
                        annotations: None,
                    },
                }],
            })
        })
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
                    },
                }],
            })
        })
}
