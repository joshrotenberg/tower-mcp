//! Validates tower-mcp protocol types against the official MCP JSON schema.
//!
//! This test suite serializes Rust types to JSON and validates them against
//! the corresponding `$defs` entries in the MCP specification schema
//! (`schema/2025-11-25/schema.json`).
//!
//! This catches: wrong field names (camelCase mismatches), missing required
//! fields, wrong types, and structural mismatches.

use serde::Serialize;
use serde_json::{Value, json};
use std::sync::LazyLock;

use tower_mcp::protocol::*;

// =============================================================================
// Schema loading and validation helpers
// =============================================================================

static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    let schema_str = std::fs::read_to_string("tests/fixtures/mcp-schema-2025-11-25.json").unwrap();
    serde_json::from_str(&schema_str).unwrap()
});

/// Validate a serialized value against a `$defs` entry in the MCP schema.
fn validate_against_def<T: Serialize>(value: &T, def_name: &str) {
    let instance = serde_json::to_value(value).unwrap();

    // Build a sub-schema that references the specific $def
    let sub_schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": format!("#/$defs/{}", def_name),
        "$defs": SCHEMA["$defs"]
    });

    let validator = jsonschema::validator_for(&sub_schema)
        .unwrap_or_else(|e| panic!("Failed to compile schema for {def_name}: {e}"));

    let errors: Vec<_> = validator.iter_errors(&instance).collect();
    if !errors.is_empty() {
        let error_msgs: Vec<String> = errors.iter().map(|e| format!("  - {e}")).collect();
        panic!(
            "Schema validation failed for {def_name}:\nInstance: {}\nErrors:\n{}",
            serde_json::to_string_pretty(&instance).unwrap(),
            error_msgs.join("\n")
        );
    }
}

// =============================================================================
// Result types
// =============================================================================

#[test]
fn validate_call_tool_result() {
    let result = CallToolResult::text("hello");
    validate_against_def(&result, "CallToolResult");
}

#[test]
fn validate_call_tool_result_with_error() {
    let mut result = CallToolResult::text("something went wrong");
    result.is_error = true;
    validate_against_def(&result, "CallToolResult");
}

#[test]
fn validate_list_tools_result() {
    let result = ListToolsResult {
        tools: vec![ToolDefinition {
            name: "test-tool".to_string(),
            title: Some("Test Tool".to_string()),
            description: Some("A test tool".to_string()),
            input_schema: json!({"type": "object", "properties": {}}),
            output_schema: None,
            annotations: None,
            icons: None,
            execution: None,
            meta: None,
        }],
        next_cursor: None,
        meta: None,
    };
    validate_against_def(&result, "ListToolsResult");
}

#[test]
fn validate_list_resources_result() {
    let result = ListResourcesResult {
        resources: vec![ResourceDefinition {
            uri: "file:///test.txt".to_string(),
            name: "Test File".to_string(),
            title: None,
            description: None,
            mime_type: Some("text/plain".to_string()),
            size: None,
            annotations: None,
            icons: None,
            meta: None,
        }],
        next_cursor: None,
        meta: None,
    };
    validate_against_def(&result, "ListResourcesResult");
}

#[test]
fn validate_read_resource_result() {
    let result = ReadResourceResult {
        contents: vec![ResourceContent {
            uri: "file:///test.txt".to_string(),
            mime_type: Some("text/plain".to_string()),
            text: Some("file contents".to_string()),
            blob: None,
            meta: None,
        }],
        meta: None,
    };
    validate_against_def(&result, "ReadResourceResult");
}

#[test]
fn validate_list_prompts_result() {
    let result = ListPromptsResult {
        prompts: vec![PromptDefinition {
            name: "greeting".to_string(),
            title: None,
            description: Some("A greeting prompt".to_string()),
            arguments: vec![PromptArgument {
                name: "name".to_string(),
                description: Some("The name to greet".to_string()),
                required: true,
            }],
            icons: None,
            meta: None,
        }],
        next_cursor: None,
        meta: None,
    };
    validate_against_def(&result, "ListPromptsResult");
}

#[test]
fn validate_get_prompt_result() {
    let result = GetPromptResult {
        description: Some("A test prompt".to_string()),
        messages: vec![PromptMessage {
            role: PromptRole::User,
            content: Content::Text {
                text: "Hello!".to_string(),
                annotations: None,
                meta: None,
            },
            meta: None,
        }],
        meta: None,
    };
    validate_against_def(&result, "GetPromptResult");
}

#[test]
fn validate_initialize_result() {
    let result = InitializeResult {
        protocol_version: "2025-11-25".to_string(),
        capabilities: ServerCapabilities {
            experimental: None,
            logging: None,
            completions: None,
            prompts: None,
            resources: None,
            tools: None,
            tasks: None,
            extensions: None,
        },
        server_info: Implementation {
            name: "test-server".to_string(),
            version: "1.0.0".to_string(),
            title: None,
            description: None,
            icons: None,
            website_url: None,
            meta: None,
        },
        instructions: None,
        meta: None,
    };
    validate_against_def(&result, "InitializeResult");
}

#[test]
fn validate_complete_result() {
    let result = CompleteResult {
        completion: Completion {
            values: vec!["option1".to_string(), "option2".to_string()],
            has_more: Some(false),
            total: Some(2),
        },
        meta: None,
    };
    validate_against_def(&result, "CompleteResult");
}

#[test]
fn validate_list_resource_templates_result() {
    let result = ListResourceTemplatesResult {
        resource_templates: vec![ResourceTemplateDefinition {
            uri_template: "file:///{path}".to_string(),
            name: "Files".to_string(),
            title: None,
            description: Some("Access files".to_string()),
            mime_type: Some("text/plain".to_string()),
            annotations: None,
            icons: None,
            arguments: vec![],
            meta: None,
        }],
        next_cursor: None,
        meta: None,
    };
    validate_against_def(&result, "ListResourceTemplatesResult");
}

// =============================================================================
// Request params types
// =============================================================================

#[test]
fn validate_call_tool_request_params() {
    let params = CallToolParams {
        name: "test-tool".to_string(),
        arguments: json!({"key": "value"}),
        meta: None,
        task: None,
    };
    validate_against_def(&params, "CallToolRequestParams");
}

#[test]
fn validate_initialize_request_params() {
    let params = InitializeParams {
        protocol_version: "2025-11-25".to_string(),
        capabilities: ClientCapabilities {
            experimental: None,
            roots: None,
            sampling: None,
            elicitation: None,
            tasks: None,
            extensions: None,
        },
        client_info: Implementation {
            name: "test-client".to_string(),
            version: "1.0.0".to_string(),
            title: None,
            description: None,
            icons: None,
            website_url: None,
            meta: None,
        },
        meta: None,
    };
    validate_against_def(&params, "InitializeRequestParams");
}

#[test]
fn validate_get_prompt_request_params() {
    let params = GetPromptParams {
        name: "greeting".to_string(),
        arguments: std::collections::HashMap::from([("name".to_string(), "world".to_string())]),
        meta: None,
    };
    validate_against_def(&params, "GetPromptRequestParams");
}

#[test]
fn validate_read_resource_request_params() {
    let params = ReadResourceParams {
        uri: "file:///test.txt".to_string(),
        meta: None,
    };
    validate_against_def(&params, "ReadResourceRequestParams");
}

#[test]
fn validate_set_level_request_params() {
    // SetLogLevelParams only derives Deserialize, so use json! directly
    let params = json!({"level": "warning"});
    validate_against_def(&params, "SetLevelRequestParams");
}

#[test]
fn validate_subscribe_request_params() {
    // SubscribeResourceParams only derives Deserialize, so use json! directly
    let params = json!({"uri": "file:///test.txt"});
    validate_against_def(&params, "SubscribeRequestParams");
}

#[test]
fn validate_unsubscribe_request_params() {
    // UnsubscribeResourceParams only derives Deserialize, so use json! directly
    let params = json!({"uri": "file:///test.txt"});
    validate_against_def(&params, "UnsubscribeRequestParams");
}

// =============================================================================
// Paginated params (ListTools, ListResources, ListPrompts, ListResourceTemplates)
// =============================================================================

#[test]
fn validate_list_tools_params_as_paginated() {
    let params = ListToolsParams {
        cursor: None,
        meta: None,
    };
    validate_against_def(&params, "PaginatedRequestParams");
}

#[test]
fn validate_list_tools_params_with_cursor() {
    let params = ListToolsParams {
        cursor: Some("page2".to_string()),
        meta: None,
    };
    validate_against_def(&params, "PaginatedRequestParams");
}

#[test]
fn validate_list_resources_params_as_paginated() {
    let params = ListResourcesParams {
        cursor: None,
        meta: None,
    };
    validate_against_def(&params, "PaginatedRequestParams");
}

#[test]
fn validate_list_prompts_params_as_paginated() {
    let params = ListPromptsParams {
        cursor: None,
        meta: None,
    };
    validate_against_def(&params, "PaginatedRequestParams");
}

#[test]
fn validate_list_resource_templates_params_as_paginated() {
    // ListResourceTemplatesParams only derives Deserialize, so use json! directly
    let params = json!({});
    validate_against_def(&params, "PaginatedRequestParams");
}

// =============================================================================
// Core object types
// =============================================================================

#[test]
fn validate_tool_definition() {
    let tool = ToolDefinition {
        name: "search".to_string(),
        title: Some("Search".to_string()),
        description: Some("Search for items".to_string()),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"}
            },
            "required": ["query"]
        }),
        output_schema: None,
        annotations: Some(ToolAnnotations {
            title: Some("Search Tool".to_string()),
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        }),
        icons: None,
        execution: None,
        meta: None,
    };
    validate_against_def(&tool, "Tool");
}

#[test]
fn validate_tool_with_execution() {
    let tool = ToolDefinition {
        name: "long-task".to_string(),
        title: None,
        description: Some("A long-running task".to_string()),
        input_schema: json!({"type": "object"}),
        output_schema: None,
        annotations: None,
        icons: None,
        execution: Some(ToolExecution {
            task_support: Some(TaskSupportMode::Optional),
        }),
        meta: None,
    };
    validate_against_def(&tool, "Tool");
}

#[test]
fn validate_implementation() {
    let impl_ = Implementation {
        name: "my-server".to_string(),
        version: "2.0.0".to_string(),
        title: Some("My Server".to_string()),
        description: Some("A test server".to_string()),
        icons: None,
        website_url: Some("https://example.com".to_string()),
        meta: None,
    };
    validate_against_def(&impl_, "Implementation");
}

#[test]
fn validate_resource_definition() {
    let resource = ResourceDefinition {
        uri: "file:///data.json".to_string(),
        name: "Data File".to_string(),
        title: Some("Data".to_string()),
        description: Some("JSON data file".to_string()),
        mime_type: Some("application/json".to_string()),
        size: Some(1024),
        annotations: Some(ContentAnnotations {
            audience: Some(vec![ContentRole::User]),
            priority: Some(0.8),
            last_modified: None,
        }),
        icons: None,
        meta: None,
    };
    validate_against_def(&resource, "Resource");
}

#[test]
fn validate_prompt_definition() {
    let prompt = PromptDefinition {
        name: "summarize".to_string(),
        title: Some("Summarize".to_string()),
        description: Some("Summarize text".to_string()),
        arguments: vec![
            PromptArgument {
                name: "text".to_string(),
                description: Some("The text to summarize".to_string()),
                required: true,
            },
            PromptArgument {
                name: "max_length".to_string(),
                description: Some("Maximum summary length".to_string()),
                required: false,
            },
        ],
        icons: None,
        meta: None,
    };
    validate_against_def(&prompt, "Prompt");
}

#[test]
fn validate_resource_template() {
    let template = ResourceTemplateDefinition {
        uri_template: "db://users/{id}".to_string(),
        name: "User Record".to_string(),
        title: None,
        description: Some("Fetch a user by ID".to_string()),
        mime_type: Some("application/json".to_string()),
        annotations: None,
        icons: None,
        arguments: vec![],
        meta: None,
    };
    validate_against_def(&template, "ResourceTemplate");
}

// =============================================================================
// Content types
// =============================================================================

#[test]
fn validate_text_content() {
    let content = Content::text("Hello, world!");
    validate_against_def(&content, "TextContent");
}

#[test]
fn validate_image_content() {
    let content = Content::Image {
        data: "iVBORw0KGgo=".to_string(),
        mime_type: "image/png".to_string(),
        annotations: None,
        meta: None,
    };
    validate_against_def(&content, "ImageContent");
}

#[test]
fn validate_audio_content() {
    let content = Content::Audio {
        data: "AAAA".to_string(),
        mime_type: "audio/wav".to_string(),
        annotations: None,
        meta: None,
    };
    validate_against_def(&content, "AudioContent");
}

#[test]
fn validate_embedded_resource_content() {
    let content = Content::Resource {
        resource: ResourceContent {
            uri: "file:///test.txt".to_string(),
            mime_type: Some("text/plain".to_string()),
            text: Some("file contents".to_string()),
            blob: None,
            meta: None,
        },
        annotations: None,
        meta: None,
    };
    validate_against_def(&content, "EmbeddedResource");
}

#[test]
fn validate_resource_link_content() {
    let content = Content::ResourceLink {
        uri: "file:///test.txt".to_string(),
        name: "Test File".to_string(),
        title: Some("Test".to_string()),
        description: None,
        mime_type: Some("text/plain".to_string()),
        size: Some(42),
        annotations: None,
        icons: None,
        meta: None,
    };
    validate_against_def(&content, "ResourceLink");
}

// =============================================================================
// Notification params
// =============================================================================

#[test]
fn validate_progress_notification_params() {
    let params = ProgressParams {
        progress_token: ProgressToken::String("tok-1".to_string()),
        progress: 50.0,
        total: Some(100.0),
        message: Some("Processing...".to_string()),
        meta: None,
    };
    validate_against_def(&params, "ProgressNotificationParams");
}

#[test]
fn validate_logging_message_notification_params() {
    let params = LoggingMessageParams {
        level: LogLevel::Info,
        logger: Some("app".to_string()),
        data: json!("Server started"),
        meta: None,
    };
    validate_against_def(&params, "LoggingMessageNotificationParams");
}

#[test]
fn validate_resource_updated_notification_params() {
    let params = json!({"uri": "file:///data.json"});
    validate_against_def(&params, "ResourceUpdatedNotificationParams");
}

// =============================================================================
// Task types
// =============================================================================

#[test]
fn validate_task_object() {
    // Note: MCP schema says ttl is required + type "integer", but description says
    // "null for unlimited". Using a concrete value here to avoid the spec inconsistency.
    let task = TaskObject {
        task_id: "task-001".to_string(),
        status: TaskStatus::Working,
        status_message: Some("In progress".to_string()),
        created_at: "2025-01-01T00:00:00Z".to_string(),
        last_updated_at: "2025-01-01T00:01:00Z".to_string(),
        ttl: Some(300_000),
        poll_interval: None,
        meta: None,
    };
    validate_against_def(&task, "Task");
}

#[test]
fn validate_task_completed() {
    let task = TaskObject {
        task_id: "task-002".to_string(),
        status: TaskStatus::Completed,
        status_message: None,
        created_at: "2025-01-01T00:00:00Z".to_string(),
        last_updated_at: "2025-01-01T00:05:00Z".to_string(),
        ttl: Some(600_000),
        poll_interval: Some(1000),
        meta: None,
    };
    validate_against_def(&task, "Task");
}

// =============================================================================
// Sampling types
// =============================================================================

#[test]
fn validate_create_message_result() {
    let result = CreateMessageResult {
        content: SamplingContentOrArray::Single(SamplingContent::Text {
            text: "Hello!".to_string(),
            annotations: None,
            meta: None,
        }),
        model: "claude-3-sonnet".to_string(),
        role: ContentRole::Assistant,
        stop_reason: Some("endTurn".to_string()),
        meta: None,
    };
    validate_against_def(&result, "CreateMessageResult");
}

// =============================================================================
// Capabilities
// =============================================================================

#[test]
fn validate_server_capabilities() {
    let caps = ServerCapabilities {
        experimental: None,
        logging: Some(LoggingCapability {}),
        completions: None,
        prompts: Some(PromptsCapability { list_changed: true }),
        resources: Some(ResourcesCapability {
            subscribe: true,
            list_changed: true,
        }),
        tools: Some(ToolsCapability { list_changed: true }),
        tasks: None,
        extensions: None,
    };
    validate_against_def(&caps, "ServerCapabilities");
}

#[test]
fn validate_client_capabilities() {
    let caps = ClientCapabilities {
        experimental: None,
        roots: Some(RootsCapability { list_changed: true }),
        sampling: Some(SamplingCapability {
            tools: None,
            context: None,
        }),
        elicitation: Some(ElicitationCapability {
            form: None,
            url: None,
        }),
        tasks: None,
        extensions: None,
    };
    validate_against_def(&caps, "ClientCapabilities");
}

// =============================================================================
// Elicitation schema types
// =============================================================================

#[test]
fn validate_string_schema() {
    let schema = StringSchema {
        schema_type: "string".to_string(),
        title: None,
        description: Some("Your name".to_string()),
        format: None,
        pattern: None,
        min_length: None,
        max_length: Some(100),
        default: None,
    };
    validate_against_def(&schema, "StringSchema");
}

#[test]
fn validate_number_schema() {
    let schema = NumberSchema {
        schema_type: "number".to_string(),
        title: None,
        description: Some("Your age".to_string()),
        minimum: Some(0.0),
        maximum: Some(150.0),
        default: None,
    };
    validate_against_def(&schema, "NumberSchema");
}

#[test]
fn validate_boolean_schema() {
    let schema = BooleanSchema {
        schema_type: "boolean".to_string(),
        title: None,
        description: Some("Accept terms?".to_string()),
        default: None,
    };
    validate_against_def(&schema, "BooleanSchema");
}
