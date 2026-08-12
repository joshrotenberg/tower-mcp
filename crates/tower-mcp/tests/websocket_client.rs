//! Round-trip coverage for `WebSocketClientTransport` against the crate's own
//! `WebSocketTransport` server (#1032).

#![cfg(feature = "websocket")]

use std::time::Duration;

use tower_mcp::client::{McpClient, WebSocketClientConfig, WebSocketClientTransport};
use tower_mcp::{CallToolResult, McpRouter, PromptBuilder, ResourceBuilder, ToolBuilder};

fn router() -> McpRouter {
    let echo = ToolBuilder::new("echo")
        .description("Echo a value")
        .handler(|v: serde_json::Value| async move { Ok(CallToolResult::text(v.to_string())) })
        .build();
    // `ResourceBuilder` finishes at the content method; there is no `build`.
    let resource = ResourceBuilder::new("mem://one").name("one").text("hello");
    let prompt = PromptBuilder::new("greet")
        .description("Greet")
        .handler(
            |_args: std::collections::HashMap<String, String>| async move {
                Ok(tower_mcp::GetPromptResult {
                    description: None,
                    messages: vec![tower_mcp::protocol::PromptMessage {
                        role: tower_mcp::protocol::PromptRole::User,
                        content: tower_mcp::protocol::Content::Text {
                            text: "hello there".to_string(),
                            annotations: None,
                            meta: None,
                        },
                        meta: None,
                    }],
                    meta: None,
                })
            },
        )
        .build();

    McpRouter::new()
        .server_info("ws-test-server", "1.0.0")
        .tool(echo)
        .resource(resource)
        .prompt(prompt)
}

/// Start the server on an ephemeral port and return its ws:// URL.
async fn serve(router: McpRouter) -> String {
    let app = tower_mcp::WebSocketTransport::new(router).into_router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    // Let the listener come up before the first dial.
    tokio::time::sleep(Duration::from_millis(50)).await;
    format!("ws://{addr}/")
}

/// The acceptance criterion: a client drives the full surface over a socket.
#[tokio::test]
async fn the_full_surface_round_trips_over_a_websocket() {
    let url = serve(router()).await;
    let transport = WebSocketClientTransport::connect(&url)
        .await
        .expect("connect");
    let client = McpClient::connect(transport).await.expect("client");

    let initialized = client.initialize("ws-client", "1.0.0").await.expect("init");
    assert_eq!(initialized.server_info.name, "ws-test-server");

    let tools = client.list_tools().await.expect("tools");
    assert!(tools.tools.iter().any(|t| t.name == "echo"));

    let result = client
        .call_tool("echo", serde_json::json!({"v": 1}))
        .await
        .expect("call");
    assert!(result.all_text().contains("\"v\""));

    let resources = client.list_resources().await.expect("resources");
    assert_eq!(resources.resources.len(), 1);

    let read = client.read_resource("mem://one").await.expect("read");
    assert!(!read.contents.is_empty());

    let prompts = client.list_prompts().await.expect("prompts");
    assert!(prompts.prompts.iter().any(|p| p.name == "greet"));

    client.shutdown().await.expect("shutdown");
}

/// A version subprotocol is offered and honored, so a client can pin the
/// revision it speaks rather than accepting the server's default.
#[tokio::test]
async fn a_requested_protocol_version_is_negotiated() {
    let url = serve(router()).await;
    let transport = WebSocketClientTransport::connect_with_config(
        &url,
        WebSocketClientConfig {
            protocol_version: Some("2025-11-25".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("connect");

    let client = McpClient::connect(transport).await.expect("client");
    let initialized = client.initialize("ws-client", "1.0.0").await.expect("init");
    assert_eq!(initialized.protocol_version, "2025-11-25");
    client.shutdown().await.expect("shutdown");
}

/// Bearer and custom headers ride the opening handshake without breaking it.
#[tokio::test]
async fn bearer_and_custom_headers_are_accepted() {
    let url = serve(router()).await;
    let transport = WebSocketClientTransport::connect_with_config(
        &url,
        WebSocketClientConfig {
            bearer: Some("test-token".to_string()),
            headers: vec![("x-example".to_string(), "1".to_string())],
            ..Default::default()
        },
    )
    .await
    .expect("connect with auth");

    let client = McpClient::connect(transport).await.expect("client");
    client.initialize("ws-client", "1.0.0").await.expect("init");
    client.shutdown().await.expect("shutdown");
}

/// A dropped socket ends the conversation rather than being retried: there is
/// no session id and no replay, so a reconnect would start a new one.
#[tokio::test]
async fn a_closed_socket_reports_disconnected() {
    let url = serve(router()).await;
    let mut transport = WebSocketClientTransport::connect(&url)
        .await
        .expect("connect");

    use tower_mcp::client::ClientTransport;
    assert!(transport.is_connected());
    assert!(
        !transport.supports_session_recovery(),
        "a WebSocket cannot resume, so the client must not try"
    );

    transport.close().await.expect("close");
    assert!(!transport.is_connected());
}

/// An unreachable endpoint fails at connect rather than surfacing later as a
/// confusing protocol error.
#[tokio::test]
async fn connecting_to_a_dead_endpoint_fails_immediately() {
    let error = match WebSocketClientTransport::connect("ws://127.0.0.1:1/").await {
        Ok(_) => panic!("connecting to a dead endpoint must fail"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("WebSocket connect failed"),
        "the error should name the stage that failed: {error}"
    );
}

/// #1303: a server whose runtime prefixes its output with a UTF-8 BOM writes
/// one into its text frames too, and the JSON parser rejects the whole frame
/// over it. `trim` does not remove a BOM, so the client has to strip it the
/// way the server side already does.
#[tokio::test]
async fn a_bom_prefixed_frame_from_the_server_is_stripped() {
    use axum::extract::ws::{Message, WebSocketUpgrade};
    use tower_mcp::client::ClientTransport;

    const FRAME: &str = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;

    // A bare WebSocket endpoint that writes one BOM-prefixed frame. The
    // crate's own server never emits one, which is the point: this is what a
    // peer we do not control does.
    let app = axum::Router::new().route(
        "/",
        axum::routing::get(|upgrade: WebSocketUpgrade| async move {
            upgrade.on_upgrade(|mut socket| async move {
                let _ = socket
                    .send(Message::Text(format!("\u{feff}{FRAME}").into()))
                    .await;
            })
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut transport = WebSocketClientTransport::connect(&format!("ws://{addr}/"))
        .await
        .expect("connect");
    let received = tokio::time::timeout(Duration::from_secs(5), transport.recv())
        .await
        .expect("a frame must arrive")
        .expect("recv")
        .expect("the frame must not be dropped");

    assert_eq!(received, FRAME);
    serde_json::from_str::<serde_json::Value>(&received)
        .expect("and it must parse, which is the point");
}
