//! Over Streamable HTTP, a method whose capability the server did not
//! advertise answers -32601 on both lifecycles (#1654).
//!
//! This is what the conformance check `sep-2575-server-declares-prompts-in-discover`
//! (rmcp #1445) observes: a tools-only server must not answer `prompts/list`
//! with an empty success. On the 2026-07-28 path -32601 is also HTTP 404.

#![cfg(feature = "http")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

fn tools_only_app() -> axum::Router {
    let router = McpRouter::new().server_info("tools-only", "1.0.0").tool(
        ToolBuilder::new("echo")
            .description("Echo the arguments")
            .handler(
                |args: serde_json::Value| async move { Ok(CallToolResult::text(args.to_string())) },
            )
            .build(),
    );
    HttpTransport::new(router)
        .disable_origin_validation()
        .into_router()
}

/// Methods a tools-only server does not advertise, with their parameters.
fn unadvertised() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("prompts/list", serde_json::json!({})),
        ("resources/list", serde_json::json!({})),
        ("resources/templates/list", serde_json::json!({})),
        (
            "completion/complete",
            serde_json::json!({
                "ref": {"type": "ref/prompt", "name": "greet"},
                "argument": {"name": "topic", "value": "a"}
            }),
        ),
    ]
}

async fn post(
    app: &axum::Router,
    body: serde_json::Value,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let response_headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let json = serde_json::from_str(&text).unwrap_or_else(|_| {
        match text.lines().find_map(|line| line.strip_prefix("data: ")) {
            Some(data) => serde_json::from_str(data).expect("SSE data is JSON"),
            None => serde_json::Value::Null,
        }
    });
    (status, response_headers, json)
}

#[tokio::test]
async fn a_2025_session_gets_method_not_found_for_unadvertised_capabilities() {
    let app = tools_only_app();
    let (_, headers, init) = post(
        &app,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "probe", "version": "1.0"}
            }
        }),
        &[],
    )
    .await;
    let capabilities = &init["result"]["capabilities"];
    assert!(capabilities["tools"].is_object(), "{init}");
    for key in ["prompts", "resources", "completions"] {
        assert!(capabilities.get(key).is_none(), "{key}: {init}");
    }
    let session = headers
        .get("mcp-session-id")
        .expect("session id")
        .to_str()
        .unwrap()
        .to_string();
    let session_headers = [
        ("mcp-session-id", session.as_str()),
        ("mcp-protocol-version", "2025-11-25"),
    ];
    post(
        &app,
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        &session_headers,
    )
    .await;

    let (_, _, tools) = post(
        &app,
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        &session_headers,
    )
    .await;
    assert_eq!(tools["result"]["tools"][0]["name"], "echo", "{tools}");

    for (id, (method, params)) in (3..).zip(unadvertised()) {
        let (_, _, response) = post(
            &app,
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            &session_headers,
        )
        .await;
        assert_eq!(response["id"], id, "{response}");
        assert_eq!(response["error"]["code"], -32601, "{method}: {response}");
    }
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn a_2026_07_28_request_gets_method_not_found_for_unadvertised_capabilities() {
    let app = tools_only_app();
    let meta = serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    });

    let (status, _, discover) = post(
        &app,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "server/discover",
            "params": {"_meta": meta}
        }),
        &[
            ("mcp-protocol-version", "2026-07-28"),
            ("mcp-method", "server/discover"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{discover}");
    let capabilities = &discover["result"]["capabilities"];
    assert!(capabilities["tools"].is_object(), "{discover}");
    for key in ["prompts", "resources", "completions"] {
        assert!(capabilities.get(key).is_none(), "{key}: {discover}");
    }

    let (status, _, tools) = post(
        &app,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list",
            "params": {"_meta": meta}
        }),
        &[
            ("mcp-protocol-version", "2026-07-28"),
            ("mcp-method", "tools/list"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tools}");
    assert_eq!(tools["result"]["tools"][0]["name"], "echo", "{tools}");

    for (id, (method, mut params)) in (3..).zip(unadvertised()) {
        params["_meta"] = meta.clone();
        let (status, _, response) = post(
            &app,
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            &[
                ("mcp-protocol-version", "2026-07-28"),
                ("mcp-method", method),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method}: {response}");
        assert_eq!(response["id"], id, "{response}");
        assert_eq!(response["error"]["code"], -32601, "{method}: {response}");
    }
}
