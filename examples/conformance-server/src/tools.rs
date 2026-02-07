use base64::Engine;
use tower_mcp::protocol::{
    Content, CreateMessageParams, LogLevel, LoggingMessageParams, ResourceContent, SamplingMessage,
};
use tower_mcp::{
    CallToolResult, ElicitFormParams, ElicitFormSchema, ElicitMode, Tool, ToolBuilder,
    extract::{Context, RawArgs},
};

/// 1x1 red PNG
const RED_PIXEL_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
    0x00, 0x00, 0x03, 0x00, 0x01, 0x36, 0x28, 0x19, 0x00, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// Minimal WAV file (8kHz, 8-bit, mono, 1 sample of silence)
const MINIMAL_WAV: &[u8] = &[
    0x52, 0x49, 0x46, 0x46, // "RIFF"
    0x25, 0x00, 0x00, 0x00, // file size - 8
    0x57, 0x41, 0x56, 0x45, // "WAVE"
    0x66, 0x6D, 0x74, 0x20, // "fmt "
    0x10, 0x00, 0x00, 0x00, // chunk size (16)
    0x01, 0x00, // PCM format
    0x01, 0x00, // mono
    0x40, 0x1F, 0x00, 0x00, // 8000 Hz
    0x40, 0x1F, 0x00, 0x00, // byte rate
    0x01, 0x00, // block align
    0x08, 0x00, // bits per sample
    0x64, 0x61, 0x74, 0x61, // "data"
    0x01, 0x00, 0x00, 0x00, // data size (1 byte)
    0x80, // one sample of silence
];

pub fn red_pixel_base64() -> String {
    base64::engine::general_purpose::STANDARD.encode(RED_PIXEL_PNG)
}

fn minimal_wav_base64() -> String {
    base64::engine::general_purpose::STANDARD.encode(MINIMAL_WAV)
}

/// Build all conformance tools.
pub fn build_tools() -> Vec<Tool> {
    vec![
        build_simple_text(),
        build_image_content(),
        build_audio_content(),
        build_embedded_resource(),
        build_multiple_content_types(),
        build_tool_with_logging(),
        build_tool_with_progress(),
        build_error_handling(),
        build_sampling(),
        build_elicitation(),
        build_elicitation_sep1034_defaults(),
        build_elicitation_sep1330_enums(),
    ]
}

fn build_simple_text() -> Tool {
    ToolBuilder::new("test_simple_text")
        .description("Returns simple text content")
        .extractor_handler((), |RawArgs(_args): RawArgs| async move {
            Ok(CallToolResult::text(
                "This is a simple text response for testing.",
            ))
        })
        .build()
}

fn build_image_content() -> Tool {
    ToolBuilder::new("test_image_content")
        .description("Returns image content (1x1 red PNG)")
        .extractor_handler((), |RawArgs(_args): RawArgs| async move {
            Ok(CallToolResult::image(red_pixel_base64(), "image/png"))
        })
        .build()
}

fn build_audio_content() -> Tool {
    ToolBuilder::new("test_audio_content")
        .description("Returns audio content (minimal WAV)")
        .extractor_handler((), |RawArgs(_args): RawArgs| async move {
            Ok(CallToolResult::audio(minimal_wav_base64(), "audio/wav"))
        })
        .build()
}

fn build_embedded_resource() -> Tool {
    ToolBuilder::new("test_embedded_resource")
        .description("Returns an embedded resource")
        .extractor_handler((), |RawArgs(_args): RawArgs| async move {
            Ok(CallToolResult::resource(ResourceContent {
                uri: "test://embedded-resource".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: Some("Embedded resource content".to_string()),
                blob: None,
            }))
        })
        .build()
}

fn build_multiple_content_types() -> Tool {
    ToolBuilder::new("test_multiple_content_types")
        .description("Returns multiple content types (text + image + resource)")
        .extractor_handler((), |RawArgs(_args): RawArgs| async move {
            Ok(CallToolResult {
                content: vec![
                    Content::Text {
                        text: "This is text content".to_string(),
                        annotations: None,
                    },
                    Content::Image {
                        data: red_pixel_base64(),
                        mime_type: "image/png".to_string(),
                        annotations: None,
                    },
                    Content::Resource {
                        resource: ResourceContent {
                            uri: "test://embedded-resource".to_string(),
                            mime_type: Some("text/plain".to_string()),
                            text: Some("Embedded resource content".to_string()),
                            blob: None,
                        },
                        annotations: None,
                    },
                ],
                is_error: false,
                structured_content: None,
            })
        })
        .build()
}

fn build_tool_with_logging() -> Tool {
    ToolBuilder::new("test_tool_with_logging")
        .description("Sends log notifications then returns text")
        .extractor_handler((), |ctx: Context, RawArgs(_args): RawArgs| async move {
            ctx.send_log(
                LoggingMessageParams::new(LogLevel::Info)
                    .with_logger("conformance")
                    .with_data(serde_json::json!("Tool execution started")),
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            ctx.send_log(
                LoggingMessageParams::new(LogLevel::Info)
                    .with_logger("conformance")
                    .with_data(serde_json::json!("Tool processing data")),
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            ctx.send_log(
                LoggingMessageParams::new(LogLevel::Info)
                    .with_logger("conformance")
                    .with_data(serde_json::json!("Tool execution completed")),
            );
            Ok(CallToolResult::text("Logging complete"))
        })
        .build()
}

fn build_tool_with_progress() -> Tool {
    ToolBuilder::new("test_tool_with_progress")
        .description("Sends progress notifications (0/50/100) then returns text")
        .extractor_handler((), |ctx: Context, RawArgs(_args): RawArgs| async move {
            ctx.report_progress(0.0, Some(100.0), Some("Starting"))
                .await;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            ctx.report_progress(50.0, Some(100.0), Some("Halfway"))
                .await;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            ctx.report_progress(100.0, Some(100.0), Some("Complete"))
                .await;
            // Allow the last notification to be delivered via SSE before the
            // tool response is sent back on the POST response.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Ok(CallToolResult::text("Progress complete"))
        })
        .build()
}

fn build_error_handling() -> Tool {
    ToolBuilder::new("test_error_handling")
        .description("Returns an error result")
        .extractor_handler((), |RawArgs(_args): RawArgs| async move {
            Ok(CallToolResult::error("This is an error response"))
        })
        .build()
}

fn build_sampling() -> Tool {
    ToolBuilder::new("test_sampling")
        .description("Requests LLM sampling via context and returns the response")
        .extractor_handler((), |ctx: Context, RawArgs(_args): RawArgs| async move {
            if !ctx.can_sample() {
                return Ok(CallToolResult::error("Sampling not available"));
            }
            let params =
                CreateMessageParams::new(vec![SamplingMessage::user("Test sampling request")], 100);
            match ctx.sample(params).await {
                Ok(result) => {
                    // Get the first content item
                    let text = match result.content_items().first() {
                        Some(tower_mcp::SamplingContent::Text { text }) => text.clone(),
                        Some(content) => format!("{:?}", content),
                        None => "No content".to_string(),
                    };
                    Ok(CallToolResult::text(text))
                }
                Err(e) => Ok(CallToolResult::error(format!("Sampling failed: {}", e))),
            }
        })
        .build()
}

fn build_elicitation() -> Tool {
    ToolBuilder::new("test_elicitation")
        .description("Requests elicitation via context and returns the response")
        .extractor_handler((), |ctx: Context, RawArgs(_args): RawArgs| async move {
            if !ctx.can_elicit() {
                return Ok(CallToolResult::error("Elicitation not available"));
            }
            let params = ElicitFormParams {
                mode: ElicitMode::Form,
                message: "Please provide test input".to_string(),
                requested_schema: ElicitFormSchema::new().string_field(
                    "test_field",
                    Some("A test field"),
                    true,
                ),
                meta: None,
            };
            match ctx.elicit_form(params).await {
                Ok(result) => Ok(CallToolResult::text(format!(
                    "action={:?} content={:?}",
                    result.action, result.content
                ))),
                Err(e) => Ok(CallToolResult::error(format!("Elicitation failed: {}", e))),
            }
        })
        .build()
}

fn build_elicitation_sep1034_defaults() -> Tool {
    ToolBuilder::new("test_elicitation_sep1034_defaults")
        .description("Requests elicitation with default values for all primitive types")
        .extractor_handler((), |ctx: Context, RawArgs(_args): RawArgs| async move {
            if !ctx.can_elicit() {
                return Ok(CallToolResult::error("Elicitation not available"));
            }
            let params = ElicitFormParams {
                mode: ElicitMode::Form,
                message: "Please provide details with defaults".to_string(),
                requested_schema: ElicitFormSchema::new()
                    .string_field_with_default("name", Some("Name"), true, "John Doe")
                    .integer_field_with_default("age", Some("Age"), false, 30)
                    .number_field_with_default("score", Some("Score"), false, 95.5)
                    .enum_field_with_default(
                        "status",
                        Some("Status"),
                        false,
                        &["active", "inactive", "pending"],
                        "active",
                    )
                    .boolean_field_with_default("verified", Some("Verified"), false, true),
                meta: None,
            };
            match ctx.elicit_form(params).await {
                Ok(result) => Ok(CallToolResult::text(format!(
                    "Elicitation completed: action={:?}, content={:?}",
                    result.action, result.content
                ))),
                Err(e) => Ok(CallToolResult::error(format!("Elicitation failed: {}", e))),
            }
        })
        .build()
}

fn build_elicitation_sep1330_enums() -> Tool {
    ToolBuilder::new("test_elicitation_sep1330_enums")
        .description("Requests elicitation with enum schemas")
        .extractor_handler((), |ctx: Context, RawArgs(_args): RawArgs| async move {
            if !ctx.can_elicit() {
                return Ok(CallToolResult::error("Elicitation not available"));
            }
            // Build schema with all 5 enum variants
            let schema = ElicitFormSchema::new()
                // 1. Untitled single-select
                .raw_field(
                    "untitledSingle",
                    serde_json::json!({
                        "type": "string",
                        "enum": ["option1", "option2", "option3"]
                    }),
                    false,
                )
                // 2. Titled single-select
                .raw_field(
                    "titledSingle",
                    serde_json::json!({
                        "type": "string",
                        "oneOf": [
                            { "const": "value1", "title": "First Option" },
                            { "const": "value2", "title": "Second Option" },
                            { "const": "value3", "title": "Third Option" }
                        ]
                    }),
                    false,
                )
                // 3. Legacy titled (deprecated)
                .raw_field(
                    "legacyEnum",
                    serde_json::json!({
                        "type": "string",
                        "enum": ["opt1", "opt2", "opt3"],
                        "enumNames": ["Option One", "Option Two", "Option Three"]
                    }),
                    false,
                )
                // 4. Untitled multi-select
                .raw_field(
                    "untitledMulti",
                    serde_json::json!({
                        "type": "array",
                        "items": {
                            "type": "string",
                            "enum": ["option1", "option2", "option3"]
                        }
                    }),
                    false,
                )
                // 5. Titled multi-select
                .raw_field(
                    "titledMulti",
                    serde_json::json!({
                        "type": "array",
                        "items": {
                            "anyOf": [
                                { "const": "value1", "title": "First Choice" },
                                { "const": "value2", "title": "Second Choice" },
                                { "const": "value3", "title": "Third Choice" }
                            ]
                        }
                    }),
                    false,
                );
            let params = ElicitFormParams {
                mode: ElicitMode::Form,
                message: "Please select from enum options".to_string(),
                requested_schema: schema,
                meta: None,
            };
            match ctx.elicit_form(params).await {
                Ok(result) => Ok(CallToolResult::text(format!(
                    "Elicitation completed: action={:?}, content={:?}",
                    result.action, result.content
                ))),
                Err(e) => Ok(CallToolResult::error(format!("Elicitation failed: {}", e))),
            }
        })
        .build()
}
