//! Integration tests for HttpClientTransport.
//!
//! These tests spin up an in-process HTTP server and connect to it
//! using HttpClientTransport + McpClient.

#![cfg(all(feature = "http", feature = "http-client"))]

use schemars::JsonSchema;
use serde::Deserialize;
use std::time::Duration;
use tower_mcp::{
    CallToolResult, HttpTransport, McpRouter, ResourceBuilder, ToolBuilder,
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
