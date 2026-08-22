//! Regression coverage for the literal 2026-07-28 curl bodies documented by
//! `examples/http_server.rs`.

#![cfg(all(feature = "http", feature = "stateless"))]

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use tower_mcp::{
    CallToolResult, HttpTransport, McpRouter, PromptBuilder, ResourceBuilder, ToolBuilder,
};

const HTTP_SERVER_SOURCE: &str = include_str!("../../../examples/http_server.rs");
const HTTP_AUTH_SOURCE: &str = include_str!("../../../examples/http_auth.rs");

fn documented_final_request_bodies() -> Vec<Value> {
    let (_, final_section) = HTTP_SERVER_SOURCE
        .split_once("//! ## 2026-07-28 stateless flow")
        .expect("http_server example must retain its final-protocol section");
    let (_, command_block) = final_section
        .split_once("//! ```bash\n")
        .expect("final-protocol section must contain a bash block");
    let (command_block, _) = command_block
        .split_once("//! ```\n")
        .expect("final-protocol bash block must be closed");

    command_block
        .lines()
        .filter_map(|line| line.strip_prefix("//!   -d '"))
        .map(|body| {
            let body = body
                .strip_suffix('\'')
                .expect("documented curl body must close its single quote");
            serde_json::from_str(body).expect("documented curl body must be valid JSON")
        })
        .collect()
}

fn documented_auth_final_request_body() -> Value {
    let (_, final_section) = HTTP_AUTH_SOURCE
        .split_once("//! # Stateless request with API key auth (2026-07-28)")
        .expect("http_auth example must retain its final-protocol request");
    let body = final_section
        .lines()
        .find_map(|line| line.strip_prefix("//!   -d '"))
        .expect("http_auth final curl command must contain a request body")
        .strip_suffix('\'')
        .expect("documented auth curl body must close its single quote");
    serde_json::from_str(body).expect("documented auth curl body must be valid JSON")
}

fn app() -> (axum::Router, tower_mcp::SessionHandle) {
    let add = ToolBuilder::new("add")
        .description("Add two integers")
        .handler(|arguments: Value| async move {
            let a = arguments["a"].as_i64().unwrap_or_default();
            let b = arguments["b"].as_i64().unwrap_or_default();
            Ok(CallToolResult::text((a + b).to_string()))
        })
        .build();
    let config = ResourceBuilder::new("file:///config.json")
        .name("Configuration")
        .text("{}");
    let greet = PromptBuilder::new("greet").user_message("Hello");
    HttpTransport::new(
        McpRouter::new()
            .server_info("http-example", "1.0.0")
            .tool(add)
            .resource(config)
            .prompt(greet),
    )
    .disable_origin_validation()
    .disable_host_validation()
    .into_router_with_handle()
}

fn request(body: &Value) -> Request<Body> {
    let method = body["method"]
        .as_str()
        .expect("documented request must have a method");
    let accept = if method == "subscriptions/listen" {
        "text/event-stream"
    } else {
        "application/json"
    };
    let mut builder = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", accept)
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", method);
    if method == "tools/call" {
        builder = builder.header("Mcp-Name", "add");
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn next_data(body: &mut Body) -> String {
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
            .await
            .expect("timed out waiting for the documented listen acknowledgment")
            .expect("documented listen stream ended before its acknowledgment")
            .expect("documented listen stream returned an error");
        if let Ok(data) = frame.into_data() {
            return String::from_utf8(data.to_vec()).unwrap();
        }
    }
}

#[tokio::test]
async fn documented_final_curl_bodies_satisfy_current_http_validation() {
    let bodies = documented_final_request_bodies();
    let methods: Vec<_> = bodies
        .iter()
        .map(|body| body["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods,
        [
            "server/discover",
            "tools/list",
            "tools/call",
            "subscriptions/listen"
        ]
    );

    for body in &bodies {
        let meta = &body["params"]["_meta"];
        assert_eq!(
            meta["io.modelcontextprotocol/protocolVersion"],
            "2026-07-28"
        );
        assert_eq!(
            meta["io.modelcontextprotocol/clientInfo"],
            serde_json::json!({ "name": "curl", "version": "1.0" })
        );
        assert!(meta["io.modelcontextprotocol/clientCapabilities"].is_object());
    }
    assert_eq!(
        bodies[3]["params"]["notifications"],
        serde_json::json!({ "toolsListChanged": true })
    );

    let (app, handle) = app();
    for body in &bodies[..3] {
        let response = app.clone().oneshot(request(body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = json_body(response).await;
        assert!(
            response.get("error").is_none(),
            "documented {} request failed: {response}",
            body["method"]
        );

        match body["method"].as_str().unwrap() {
            "server/discover" => {
                assert_eq!(response["result"]["resultType"], "complete");
                assert_eq!(
                    response["result"]["supportedVersions"],
                    serde_json::json!(["2026-07-28", "2025-11-25", "2025-03-26"])
                );
                assert_eq!(response["result"]["ttlMs"], 0);
                assert_eq!(response["result"]["cacheScope"], "private");
                assert_eq!(
                    response["result"]["_meta"]["io.modelcontextprotocol/serverInfo"],
                    serde_json::json!({ "name": "http-example", "version": "1.0.0" })
                );
                assert_eq!(
                    response["result"]["capabilities"],
                    serde_json::json!({
                        "tools": { "listChanged": true },
                        "resources": { "subscribe": false, "listChanged": true },
                        "prompts": { "listChanged": true },
                        "logging": {}
                    })
                );
            }
            "tools/list" => assert!(response["result"]["tools"].is_array()),
            "tools/call" => assert_eq!(response["result"]["content"][0]["text"], "42"),
            method => panic!("unexpected documented method: {method}"),
        }
    }

    let auth_body = documented_auth_final_request_body();
    assert_eq!(auth_body["method"], "tools/list");
    assert_eq!(auth_body["params"]["_meta"], bodies[1]["params"]["_meta"]);
    let auth_response = app.clone().oneshot(request(&auth_body)).await.unwrap();
    assert_eq!(auth_response.status(), StatusCode::OK);
    let auth_response = json_body(auth_response).await;
    assert!(
        auth_response.get("error").is_none(),
        "documented authenticated final request failed validation: {auth_response}"
    );

    let response = app.oneshot(request(&bodies[3])).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .contains("text/event-stream")
    );
    let mut body = response.into_body();
    let acknowledgment = next_data(&mut body).await;
    assert!(acknowledgment.contains("notifications/subscriptions/acknowledged"));
    assert!(acknowledgment.contains("\"toolsListChanged\":true"));
    drop(body);

    assert_eq!(
        handle.session_count().await,
        0,
        "final curl requests must remain sessionless"
    );
}
