//! Integration tests for HttpClientTransport.
//!
//! These tests spin up an in-process HTTP server and connect to it
//! using HttpClientTransport + McpClient.

#![cfg(all(feature = "http", feature = "http-client"))]

use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tower_mcp::{
    CallToolResult, CompleteParams, CompleteResult, CompletionReference, Content, GetPromptResult,
    HttpTransport, McpRouter, PromptBuilder, PromptMessage, PromptRole, ReadResourceResult,
    ResourceBuilder, ResourceContent, ResourceTemplateBuilder, ToolBuilder,
    client::{HttpClientTransport, McpClient},
};

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoInput {
    message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

/// Create a test router with basic tools and resources.
fn test_router() -> McpRouter {
    let echo = ToolBuilder::new("echo")
        .description("Echo a message")
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
        .build();

    let add = ToolBuilder::new("add")
        .description("Add two numbers")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let config = ResourceBuilder::new("config://app")
        .name("App Config")
        .description("Application configuration")
        .text(r#"{"debug": true}"#);

    McpRouter::new()
        .server_info("test-http-server", "1.0.0")
        .tool(echo)
        .tool(add)
        .resource(config)
}

/// Start an HTTP server on a random available port and return the URL.
async fn start_server() -> (String, tokio::task::JoinHandle<()>) {
    let router = test_router();
    let transport = HttpTransport::new(router).disable_origin_validation();
    let axum_router = transport.into_router();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());

    let handle = tokio::spawn(async move {
        axum::serve(listener, axum_router).await.unwrap();
    });

    // Give the server a moment to start
    tokio::time::sleep(Duration::from_millis(50)).await;

    (url, handle)
}

#[tokio::test]
async fn test_http_client_initialize() {
    let (url, _server) = start_server().await;

    let transport = HttpClientTransport::new(&url);
    let client = McpClient::connect(transport).await.unwrap();

    let info = client.initialize("test-client", "1.0.0").await.unwrap();
    assert_eq!(info.server_info.name, "test-http-server");
    assert_eq!(info.server_info.version, "1.0.0");
    assert!(client.is_initialized());

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_http_client_list_tools() {
    let (url, _server) = start_server().await;

    let transport = HttpClientTransport::new(&url);
    let client = McpClient::connect(transport).await.unwrap();
    client.initialize("test-client", "1.0.0").await.unwrap();

    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.tools.len(), 2);

    let tool_names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(tool_names.contains(&"echo"));
    assert!(tool_names.contains(&"add"));

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_http_client_call_tool() {
    let (url, _server) = start_server().await;

    let transport = HttpClientTransport::new(&url);
    let client = McpClient::connect(transport).await.unwrap();
    client.initialize("test-client", "1.0.0").await.unwrap();

    // Call echo
    let result = client
        .call_tool(
            "echo",
            serde_json::json!({"message": "Hello from HTTP client!"}),
        )
        .await
        .unwrap();
    assert!(!result.content.is_empty());

    // Call add
    let result = client
        .call_tool("add", serde_json::json!({"a": 42, "b": 58}))
        .await
        .unwrap();
    assert!(!result.content.is_empty());

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_http_client_list_resources() {
    let (url, _server) = start_server().await;

    let transport = HttpClientTransport::new(&url);
    let client = McpClient::connect(transport).await.unwrap();
    client.initialize("test-client", "1.0.0").await.unwrap();

    let resources = client.list_resources().await.unwrap();
    assert_eq!(resources.resources.len(), 1);
    assert_eq!(resources.resources[0].uri, "config://app");

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_http_client_read_resource() {
    let (url, _server) = start_server().await;

    let transport = HttpClientTransport::new(&url);
    let client = McpClient::connect(transport).await.unwrap();
    client.initialize("test-client", "1.0.0").await.unwrap();

    let result = client.read_resource("config://app").await.unwrap();
    assert_eq!(result.contents.len(), 1);
    assert!(
        result.contents[0]
            .text
            .as_deref()
            .unwrap()
            .contains("debug")
    );

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_http_client_ping() {
    let (url, _server) = start_server().await;

    let transport = HttpClientTransport::new(&url);
    let client = McpClient::connect(transport).await.unwrap();
    client.initialize("test-client", "1.0.0").await.unwrap();

    client.ping().await.unwrap();

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_http_client_shutdown_sends_delete() {
    let (url, _server) = start_server().await;

    let transport = HttpClientTransport::new(&url);
    let client = McpClient::connect(transport).await.unwrap();
    client.initialize("test-client", "1.0.0").await.unwrap();

    // Shutdown should send DELETE and close cleanly
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_http_client_with_custom_config() {
    let (url, _server) = start_server().await;

    let config = tower_mcp::client::HttpClientConfig {
        request_timeout: Duration::from_secs(10),
        sse_reconnect: false,
        ..Default::default()
    };
    let transport = HttpClientTransport::with_config(&url, config);
    let client = McpClient::connect(transport).await.unwrap();

    let info = client.initialize("test-client", "1.0.0").await.unwrap();
    assert_eq!(info.server_info.name, "test-http-server");

    client.shutdown().await.unwrap();
}

/// Start an HTTP server with bearer token auth middleware on a random port.
async fn start_auth_server(valid_token: &str) -> (String, tokio::task::JoinHandle<()>) {
    use axum::{extract::Request, http::StatusCode, middleware, response::Response};

    let router = test_router();
    let transport = HttpTransport::new(router).disable_origin_validation();
    let mcp_router = transport.into_router();

    let token = valid_token.to_string();
    let app = mcp_router.layer(middleware::from_fn(
        move |request: Request, next: middleware::Next| {
            let token = token.clone();
            async move {
                let auth = request
                    .headers()
                    .get("Authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let expected = format!("Bearer {}", token);
                if auth.as_deref() == Some(&expected) {
                    Ok::<Response, (StatusCode, String)>(next.run(request).await)
                } else {
                    Err((StatusCode::UNAUTHORIZED, "Unauthorized".to_string()))
                }
            }
        },
    ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    (url, handle)
}

#[tokio::test]
async fn test_http_client_bearer_auth() {
    let (url, _server) = start_auth_server("sk-valid-key").await;

    let transport = HttpClientTransport::new(&url).bearer_token("sk-valid-key");
    let client = McpClient::connect(transport).await.unwrap();

    let info = client.initialize("test-client", "1.0.0").await.unwrap();
    assert_eq!(info.server_info.name, "test-http-server");

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_http_client_bearer_auth_rejected() {
    let (url, _server) = start_auth_server("sk-valid-key").await;

    let transport = HttpClientTransport::new(&url).bearer_token("sk-wrong-key");
    let client = McpClient::connect(transport).await.unwrap();

    let result = client.initialize("test-client", "1.0.0").await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Extended test router with prompts, resource templates, and completions
// ---------------------------------------------------------------------------

/// Create a test router with prompts, resource templates, and completions.
fn extended_test_router() -> McpRouter {
    let echo = ToolBuilder::new("echo")
        .description("Echo a message")
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
        .build();

    let add = ToolBuilder::new("add")
        .description("Add two numbers")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let config = ResourceBuilder::new("config://app")
        .name("App Config")
        .description("Application configuration")
        .text(r#"{"debug": true}"#);

    let greeting = PromptBuilder::new("greeting")
        .description("Generate a greeting")
        .required_arg("name", "Name to greet")
        .handler(|args: HashMap<String, String>| async move {
            let name = args.get("name").cloned().unwrap_or_default();
            Ok(GetPromptResult {
                description: Some("A greeting prompt".to_string()),
                messages: vec![PromptMessage {
                    role: PromptRole::User,
                    content: Content::text(format!("Hello, {}!", name)),
                    meta: None,
                }],
                meta: None,
            })
        })
        .build();

    let farewell = PromptBuilder::new("farewell")
        .description("Generate a farewell")
        .optional_arg("name", "Name to bid farewell")
        .handler(|args: HashMap<String, String>| async move {
            let name = args
                .get("name")
                .cloned()
                .unwrap_or_else(|| "friend".to_string());
            Ok(GetPromptResult {
                description: Some("A farewell prompt".to_string()),
                messages: vec![PromptMessage {
                    role: PromptRole::User,
                    content: Content::text(format!("Goodbye, {}!", name)),
                    meta: None,
                }],
                meta: None,
            })
        })
        .build();

    let file_template = ResourceTemplateBuilder::new("file:///{+path}")
        .name("Project Files")
        .description("Read project files by path")
        .mime_type("text/plain")
        .handler(|uri: String, vars: HashMap<String, String>| async move {
            let path = vars.get("path").cloned().unwrap_or_default();
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: Some("text/plain".to_string()),
                    text: Some(format!("Contents of {}", path)),
                    blob: None,
                    meta: None,
                }],
                meta: None,
            })
        });

    McpRouter::new()
        .server_info("test-extended-server", "1.0.0")
        .tool(echo)
        .tool(add)
        .resource(config)
        .prompt(greeting)
        .prompt(farewell)
        .resource_template(file_template)
        .completion_handler(|params: CompleteParams| async move {
            match &params.reference {
                CompletionReference::Prompt { name } if name == "greeting" => {
                    let prefix = &params.argument.value;
                    let names = vec!["Alice", "Bob", "Charlie"];
                    let matches: Vec<String> = names
                        .into_iter()
                        .filter(|n| n.to_lowercase().starts_with(&prefix.to_lowercase()))
                        .map(String::from)
                        .collect();
                    Ok(CompleteResult::new(matches))
                }
                CompletionReference::Resource { uri } if uri.contains("file") => {
                    let prefix = &params.argument.value;
                    let paths = vec!["src/main.rs", "src/lib.rs", "Cargo.toml"];
                    let matches: Vec<String> = paths
                        .into_iter()
                        .filter(|p| p.starts_with(prefix.as_str()))
                        .map(String::from)
                        .collect();
                    Ok(CompleteResult::new(matches))
                }
                _ => Ok(CompleteResult::new(vec![])),
            }
        })
}

/// Start an extended HTTP server with prompts, templates, and completions.
async fn start_extended_server() -> (String, tokio::task::JoinHandle<()>) {
    let router = extended_test_router();
    let transport = HttpTransport::new(router).disable_origin_validation();
    let axum_router = transport.into_router();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());

    let handle = tokio::spawn(async move {
        axum::serve(listener, axum_router).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    (url, handle)
}

// ---------------------------------------------------------------------------
// P0: Prompts E2E
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_http_client_list_prompts() {
    let (url, _server) = start_extended_server().await;

    let transport = HttpClientTransport::new(&url);
    let client = McpClient::connect(transport).await.unwrap();
    client.initialize("test-client", "1.0.0").await.unwrap();

    let prompts = client.list_prompts().await.unwrap();
    assert_eq!(prompts.prompts.len(), 2);

    let names: Vec<&str> = prompts.prompts.iter().map(|p| p.name.as_str()).collect();
    assert!(names.contains(&"greeting"));
    assert!(names.contains(&"farewell"));

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_http_client_get_prompt_with_args() {
    let (url, _server) = start_extended_server().await;

    let transport = HttpClientTransport::new(&url);
    let client = McpClient::connect(transport).await.unwrap();
    client.initialize("test-client", "1.0.0").await.unwrap();

    let mut args = HashMap::new();
    args.insert("name".to_string(), "World".to_string());
    let result = client.get_prompt("greeting", Some(args)).await.unwrap();

    assert_eq!(result.description.as_deref(), Some("A greeting prompt"));
    assert_eq!(result.messages.len(), 1);
    assert!(matches!(result.messages[0].role, PromptRole::User));

    // Verify the message content contains our argument
    let text = match &result.messages[0].content {
        Content::Text { text, .. } => text.as_str(),
        _ => panic!("Expected text content"),
    };
    assert_eq!(text, "Hello, World!");

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_http_client_get_prompt_optional_arg() {
    let (url, _server) = start_extended_server().await;

    let transport = HttpClientTransport::new(&url);
    let client = McpClient::connect(transport).await.unwrap();
    client.initialize("test-client", "1.0.0").await.unwrap();

    // Call farewell without the optional name arg
    let result = client.get_prompt("farewell", None).await.unwrap();
    assert_eq!(result.messages.len(), 1);

    let text = match &result.messages[0].content {
        Content::Text { text, .. } => text.as_str(),
        _ => panic!("Expected text content"),
    };
    assert_eq!(text, "Goodbye, friend!");

    client.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// P0: Resource Templates E2E
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_http_client_list_resource_templates() {
    let (url, _server) = start_extended_server().await;

    let transport = HttpClientTransport::new(&url);
    let client = McpClient::connect(transport).await.unwrap();
    client.initialize("test-client", "1.0.0").await.unwrap();

    let templates = client.list_resource_templates().await.unwrap();
    assert_eq!(templates.resource_templates.len(), 1);
    assert_eq!(
        templates.resource_templates[0].uri_template,
        "file:///{+path}"
    );
    assert_eq!(templates.resource_templates[0].name, "Project Files");

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_http_client_read_resource_template() {
    let (url, _server) = start_extended_server().await;

    let transport = HttpClientTransport::new(&url);
    let client = McpClient::connect(transport).await.unwrap();
    client.initialize("test-client", "1.0.0").await.unwrap();

    let result = client.read_resource("file:///src/main.rs").await.unwrap();
    assert_eq!(result.contents.len(), 1);
    assert_eq!(
        result.contents[0].text.as_deref().unwrap(),
        "Contents of src/main.rs"
    );
    assert_eq!(result.contents[0].mime_type.as_deref(), Some("text/plain"));

    client.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// P0: call_tool_text() E2E
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_http_client_call_tool_text() {
    let (url, _server) = start_extended_server().await;

    let transport = HttpClientTransport::new(&url);
    let client = McpClient::connect(transport).await.unwrap();
    client.initialize("test-client", "1.0.0").await.unwrap();

    let text = client
        .call_tool_text("echo", serde_json::json!({"message": "hello"}))
        .await
        .unwrap();
    assert_eq!(text, "hello");

    let text = client
        .call_tool_text("add", serde_json::json!({"a": 10, "b": 20}))
        .await
        .unwrap();
    assert_eq!(text, "30");

    client.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// P0: Completion API E2E
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_http_client_complete_prompt_arg() {
    let (url, _server) = start_extended_server().await;

    let transport = HttpClientTransport::new(&url);
    let client = McpClient::connect(transport).await.unwrap();
    client.initialize("test-client", "1.0.0").await.unwrap();

    // Complete with "A" prefix -- should match "Alice"
    let result = client
        .complete_prompt_arg("greeting", "name", "A")
        .await
        .unwrap();
    assert_eq!(result.completion.values, vec!["Alice"]);

    // Complete with "b" prefix -- should match "Bob" (case-insensitive)
    let result = client
        .complete_prompt_arg("greeting", "name", "b")
        .await
        .unwrap();
    assert_eq!(result.completion.values, vec!["Bob"]);

    // Complete with "z" prefix -- no matches
    let result = client
        .complete_prompt_arg("greeting", "name", "z")
        .await
        .unwrap();
    assert!(result.completion.values.is_empty());

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_http_client_complete_resource_uri() {
    let (url, _server) = start_extended_server().await;

    let transport = HttpClientTransport::new(&url);
    let client = McpClient::connect(transport).await.unwrap();
    client.initialize("test-client", "1.0.0").await.unwrap();

    // Complete with "src/" prefix
    let result = client
        .complete_resource_uri("file:///{path}", "path", "src/")
        .await
        .unwrap();
    assert_eq!(result.completion.values.len(), 2);
    assert!(
        result
            .completion
            .values
            .contains(&"src/main.rs".to_string())
    );
    assert!(result.completion.values.contains(&"src/lib.rs".to_string()));

    // Complete with "C" prefix
    let result = client
        .complete_resource_uri("file:///{path}", "path", "C")
        .await
        .unwrap();
    assert_eq!(result.completion.values, vec!["Cargo.toml"]);

    client.shutdown().await.unwrap();
}
