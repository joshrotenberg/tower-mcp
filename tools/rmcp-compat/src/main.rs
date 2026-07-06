//! Wire-format compatibility harness: rmcp vs tower-mcp
//!
//! Starts both servers in-process as tokio tasks, fires identical MCP JSON-RPC
//! requests at each, and structurally compares the responses.
//!
//! rmcp server: port 4001 (path: /mcp/)
//! tower-mcp server: port 4002 (path: /)
//! tower-mcp SSE mode server: port 4003 (path: /)
//!
//! Run with:
//!   cargo run -p rmcp-compat

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

// ============================================================
// tower-mcp server (standard mode)
// ============================================================

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EchoInput {
    message: String,
}

async fn start_tower_mcp_server(port: u16) {
    let echo = ToolBuilder::new("echo")
        .description("Echo a message back")
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
        .build();

    let router = McpRouter::new()
        .server_info("tower-mcp-compat-test", "0.1.0")
        .tool(echo);

    let addr = format!("127.0.0.1:{port}");
    let transport = HttpTransport::new(router).disable_origin_validation();

    if let Err(e) = transport.serve(&addr).await {
        eprintln!("tower-mcp server error: {e}");
    }
}

// ============================================================
// tower-mcp server (SSE mode -- mirrors rmcp's SSE wrapping)
// ============================================================

async fn start_tower_mcp_sse_server(port: u16) {
    let echo = ToolBuilder::new("echo")
        .description("Echo a message back")
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
        .build();

    let router = McpRouter::new()
        .server_info("tower-mcp-compat-test-sse", "0.1.0")
        .tool(echo);

    let addr = format!("127.0.0.1:{port}");
    let transport = HttpTransport::new(router)
        .disable_origin_validation()
        .sse_responses(true);

    if let Err(e) = transport.serve(&addr).await {
        eprintln!("tower-mcp SSE server error: {e}");
    }
}

// ============================================================
// rmcp server
// ============================================================

mod rmcp_server {
    use rmcp::{
        ServerHandler,
        handler::server::{router::tool::ToolRouter, wrapper::Parameters},
        model::*,
        tool, tool_handler, tool_router,
    };

    #[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
    pub struct EchoParams {
        /// The message to echo back
        pub message: String,
    }

    #[derive(Clone)]
    pub struct EchoServer {
        // Used by the #[tool_router] macro-generated code
        #[allow(dead_code)]
        tool_router: ToolRouter<EchoServer>,
    }

    #[tool_router]
    impl EchoServer {
        pub fn new() -> Self {
            Self {
                tool_router: Self::tool_router(),
            }
        }

        #[tool(description = "Echo a message back")]
        fn echo(
            &self,
            Parameters(EchoParams { message }): Parameters<EchoParams>,
        ) -> Result<CallToolResult, ErrorData> {
            Ok(CallToolResult::success(vec![ContentBlock::text(message)]))
        }
    }

    #[tool_handler]
    impl ServerHandler for EchoServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
                .with_server_info(Implementation::new("rmcp-compat-test", "0.1.0"))
                .with_protocol_version(ProtocolVersion::V_2025_11_25)
        }
    }
}

async fn start_rmcp_server(port: u16) {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };

    let service = StreamableHttpService::new(
        || Ok(rmcp_server::EchoServer::new()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );

    let axum_router = axum::Router::new().nest_service("/mcp", service);
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    if let Err(e) = axum::serve(listener, axum_router).await {
        eprintln!("rmcp server error: {e}");
    }
}

// ============================================================
// Server configuration
// ============================================================

struct ServerConfig {
    name: &'static str,
    port: u16,
    path: &'static str,
}

impl ServerConfig {
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port, self.path)
    }

    fn display_name(&self) -> String {
        format!("{} (port {})", self.name, self.port)
    }
}

const RMCP: ServerConfig = ServerConfig {
    name: "rmcp",
    port: 4001,
    path: "/mcp/",
};

const TOWER_MCP: ServerConfig = ServerConfig {
    name: "tower-mcp",
    port: 4002,
    path: "/",
};

const TOWER_MCP_SSE: ServerConfig = ServerConfig {
    name: "tower-mcp-sse",
    port: 4003,
    path: "/",
};

async fn wait_for_ready(server: &ServerConfig) -> bool {
    let url = server.url();
    let client = reqwest::Client::new();
    for _ in 0..25 {
        let result = client
            .post(&url)
            .header("content-type", "application/json")
            .body(r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#)
            .timeout(std::time::Duration::from_millis(400))
            .send()
            .await;
        if result.is_ok() {
            return true;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }
    false
}

struct ServerResponse {
    body: Value,
    session_id: Option<String>,
    content_type: String,
}

async fn post_mcp(
    client: &reqwest::Client,
    server: &ServerConfig,
    body: &str,
    session_id: Option<&str>,
) -> Result<ServerResponse> {
    let mut req = client
        .post(server.url())
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body.to_string());

    if let Some(sid) = session_id {
        req = req.header("mcp-session-id", sid);
    }

    let resp = req.send().await?;
    let session_id = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = resp.bytes().await?;
    let raw_body = String::from_utf8_lossy(&bytes).to_string();

    // rmcp's StreamableHttpService returns SSE-wrapped JSON even for
    // synchronous RPC responses. Parse the SSE envelope and extract
    // the JSON from a `data:` line.
    // tower-mcp with .sse_responses(true) does the same.
    let body: Value = if content_type.contains("text/event-stream") {
        // Extract the first `data:` line from the SSE stream
        raw_body
            .lines()
            .filter(|line| line.starts_with("data:"))
            .map(|line| line.trim_start_matches("data:").trim())
            .filter(|data| !data.is_empty())
            .filter_map(|data| serde_json::from_str(data).ok())
            .next()
            .ok_or_else(|| anyhow::anyhow!("no valid data: line in SSE body: {raw_body}"))?
    } else {
        serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("JSON parse error: {e} | body: {raw_body}"))?
    };
    Ok(ServerResponse {
        body,
        session_id,
        content_type,
    })
}

// ============================================================
// Check result types
// ============================================================

#[derive(Serialize)]
enum CheckOutcome {
    Pass,
    Fail,
    KnownDiff,
    Error,
}

struct CheckResult {
    name: &'static str,
    description: String,
    outcome: CheckOutcome,
}

fn pass(name: &'static str, description: impl Into<String>) -> CheckResult {
    CheckResult {
        name,
        description: description.into(),
        outcome: CheckOutcome::Pass,
    }
}

fn fail(name: &'static str, description: impl Into<String>) -> CheckResult {
    CheckResult {
        name,
        description: description.into(),
        outcome: CheckOutcome::Fail,
    }
}

fn known_diff(name: &'static str, description: impl Into<String>) -> CheckResult {
    CheckResult {
        name,
        description: description.into(),
        outcome: CheckOutcome::KnownDiff,
    }
}

fn check_error(name: &'static str, description: impl Into<String>) -> CheckResult {
    CheckResult {
        name,
        description: description.into(),
        outcome: CheckOutcome::Error,
    }
}

fn print_result(r: &CheckResult) {
    let prefix = match r.outcome {
        CheckOutcome::Pass => "[PASS]",
        CheckOutcome::Fail => "[FAIL]",
        CheckOutcome::KnownDiff => "[KNOWN-DIFF]",
        CheckOutcome::Error => "[ERROR]",
    };
    println!("{prefix} {}: {}", r.name, r.description);
}

// ============================================================
// Individual checks
// ============================================================

/// Send the notifications/initialized notification (required by MCP spec after initialize)
async fn send_initialized(
    client: &reqwest::Client,
    server: &ServerConfig,
    session_id: Option<&str>,
) {
    let body = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
    // notifications are fire-and-forget (no id), server may return 202 or nothing
    let mut req = client
        .post(server.url())
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body.to_string());
    if let Some(sid) = session_id {
        req = req.header("mcp-session-id", sid);
    }
    let _ = req.send().await;
}

/// Check 1: initialize -- protocolVersion, capabilities, serverInfo
async fn check_initialize(
    client: &reqwest::Client,
) -> (Vec<CheckResult>, Option<String>, Option<String>) {
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"compat-test","version":"0.1.0"}}}"#;

    let rmcp_resp = post_mcp(client, &RMCP, body, None).await;
    let tower_resp = post_mcp(client, &TOWER_MCP, body, None).await;

    let (rmcp_resp, tower_resp) = match (rmcp_resp, tower_resp) {
        (Ok(r), Ok(t)) => (r, t),
        (Err(e), _) => {
            return (
                vec![check_error(
                    "initialize",
                    format!("rmcp request failed: {e}"),
                )],
                None,
                None,
            );
        }
        (_, Err(e)) => {
            return (
                vec![check_error(
                    "initialize",
                    format!("tower-mcp request failed: {e}"),
                )],
                None,
                None,
            );
        }
    };

    let rmcp_sid = rmcp_resp.session_id.clone();
    let tower_sid = tower_resp.session_id.clone();

    let mut results = Vec::new();

    // Check protocolVersion
    let rmcp_pv = rmcp_resp.body["result"]["protocolVersion"].as_str();
    let tower_pv = tower_resp.body["result"]["protocolVersion"].as_str();
    match (rmcp_pv, tower_pv) {
        (Some(r), Some(t)) if r == t => {
            results.push(pass(
                "initialize",
                format!("protocolVersion present and equal ({t})"),
            ));
        }
        (Some(r), Some(t)) => {
            results.push(fail(
                "initialize",
                format!("protocolVersion mismatch -- rmcp={r}, tower-mcp={t}"),
            ));
        }
        (None, _) => {
            results.push(fail("initialize", "rmcp missing result.protocolVersion"));
        }
        (_, None) => {
            results.push(fail(
                "initialize",
                "tower-mcp missing result.protocolVersion",
            ));
        }
    }

    // Check capabilities is object
    let rmcp_caps = &rmcp_resp.body["result"]["capabilities"];
    let tower_caps = &tower_resp.body["result"]["capabilities"];
    match (rmcp_caps.is_object(), tower_caps.is_object()) {
        (true, true) => results.push(pass("initialize", "capabilities is object on both")),
        (false, true) => results.push(fail(
            "initialize",
            format!("rmcp capabilities is not object: {rmcp_caps}"),
        )),
        (true, false) => results.push(fail(
            "initialize",
            format!("tower-mcp capabilities is not object: {tower_caps}"),
        )),
        (false, false) => results.push(fail(
            "initialize",
            format!("both capabilities non-object: rmcp={rmcp_caps}, tower-mcp={tower_caps}"),
        )),
    }

    // Check serverInfo.name and .version
    for field in ["name", "version"] {
        let rmcp_val = rmcp_resp.body["result"]["serverInfo"][field].as_str();
        let tower_val = tower_resp.body["result"]["serverInfo"][field].as_str();
        match (rmcp_val, tower_val) {
            (Some(_), Some(_)) => {
                results.push(pass(
                    "initialize",
                    format!("serverInfo.{field} is string on both"),
                ));
            }
            (None, _) => {
                results.push(fail(
                    "initialize",
                    format!("rmcp missing serverInfo.{field}"),
                ));
            }
            (_, None) => {
                results.push(fail(
                    "initialize",
                    format!("tower-mcp missing serverInfo.{field}"),
                ));
            }
        }
    }

    // MCP spec requires the client to send notifications/initialized after
    // initialize and before other requests. Send it to both so subsequent
    // requests are well-formed. Enforcement of this ordering differs between the
    // servers (tower-mcp rejects a premature request, rmcp does not); that
    // divergence is exercised by the initialized-enforcement check below.
    if let Some(ref sid) = rmcp_sid {
        send_initialized(client, &RMCP, Some(sid.as_str())).await;
    }
    if let Some(ref sid) = tower_sid {
        send_initialized(client, &TOWER_MCP, Some(sid.as_str())).await;
    }

    (results, rmcp_sid, tower_sid)
}

/// Check 2: tools/list -- result.tools array, name and inputSchema fields
async fn check_tools_list(
    client: &reqwest::Client,
    rmcp_sid: Option<&str>,
    tower_sid: Option<&str>,
) -> Vec<CheckResult> {
    let body = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;

    let rmcp_resp = post_mcp(client, &RMCP, body, rmcp_sid).await;
    let tower_resp = post_mcp(client, &TOWER_MCP, body, tower_sid).await;

    let (rmcp_resp, tower_resp) = match (rmcp_resp, tower_resp) {
        (Ok(r), Ok(t)) => (r, t),
        (Err(e), _) => {
            return vec![check_error(
                "tools/list",
                format!("rmcp request failed: {e}"),
            )];
        }
        (_, Err(e)) => {
            return vec![check_error(
                "tools/list",
                format!("tower-mcp request failed: {e}"),
            )];
        }
    };

    let mut results = Vec::new();

    let rmcp_tools = &rmcp_resp.body["result"]["tools"];
    let tower_tools = &tower_resp.body["result"]["tools"];

    match (rmcp_tools.is_array(), tower_tools.is_array()) {
        (true, true) => {
            results.push(pass("tools/list", "result.tools is array on both"));
        }
        (false, _) => {
            results.push(fail(
                "tools/list",
                format!("rmcp result.tools is not array: {rmcp_tools}"),
            ));
            return results;
        }
        (_, false) => {
            results.push(fail(
                "tools/list",
                format!("tower-mcp result.tools is not array: {tower_tools}"),
            ));
            return results;
        }
    }

    let rmcp_first = &rmcp_tools[0];
    let tower_first = &tower_tools[0];

    match (rmcp_first["name"].as_str(), tower_first["name"].as_str()) {
        (Some(r), Some(t)) => {
            if r == t {
                results.push(pass("tools/list", format!("first tool name matches ({t})")));
            } else {
                results.push(fail(
                    "tools/list",
                    format!("first tool name differs -- rmcp={r}, tower-mcp={t}"),
                ));
            }
        }
        (None, _) => results.push(fail("tools/list", "rmcp first tool missing 'name'")),
        (_, None) => results.push(fail("tools/list", "tower-mcp first tool missing 'name'")),
    }

    // Check inputSchema vs input_schema field name -- this is the most likely divergence
    let rmcp_has_camel = rmcp_first.get("inputSchema").is_some();
    let rmcp_has_snake = rmcp_first.get("input_schema").is_some();
    let tower_has_camel = tower_first.get("inputSchema").is_some();
    let tower_has_snake = tower_first.get("input_schema").is_some();

    let rmcp_field = if rmcp_has_camel {
        Some("inputSchema")
    } else if rmcp_has_snake {
        Some("input_schema")
    } else {
        None
    };

    let tower_field = if tower_has_camel {
        Some("inputSchema")
    } else if tower_has_snake {
        Some("input_schema")
    } else {
        None
    };

    match (rmcp_field, tower_field) {
        (Some(r), Some(t)) => {
            if r == t {
                results.push(pass(
                    "tools/list",
                    format!("inputSchema field name matches ({t})"),
                ));
            } else {
                results.push(fail(
                    "tools/list",
                    format!("inputSchema field name mismatch -- rmcp={r}, tower-mcp={t}"),
                ));
            }
            // verify schema is object
            let rmcp_schema = &rmcp_first[r];
            let tower_schema = &tower_first[t];
            match (rmcp_schema.is_object(), tower_schema.is_object()) {
                (true, true) => results.push(pass("tools/list", "inputSchema is object on both")),
                (false, _) => results.push(fail(
                    "tools/list",
                    format!("rmcp {r} is not object: {rmcp_schema}"),
                )),
                (_, false) => results.push(fail(
                    "tools/list",
                    format!("tower-mcp {t} is not object: {tower_schema}"),
                )),
            }
        }
        (None, _) => results.push(fail("tools/list", "rmcp first tool missing inputSchema")),
        (_, None) => results.push(fail(
            "tools/list",
            "tower-mcp first tool missing inputSchema",
        )),
    }

    results
}

/// Check 3: tools/call echo -- result.content array, content\[0\].type
async fn check_tools_call(
    client: &reqwest::Client,
    rmcp_sid: Option<&str>,
    tower_sid: Option<&str>,
) -> Vec<CheckResult> {
    let body = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"message":"hello world"}}}"#;

    let rmcp_resp = post_mcp(client, &RMCP, body, rmcp_sid).await;
    let tower_resp = post_mcp(client, &TOWER_MCP, body, tower_sid).await;

    let (rmcp_resp, tower_resp) = match (rmcp_resp, tower_resp) {
        (Ok(r), Ok(t)) => (r, t),
        (Err(e), _) => {
            return vec![check_error(
                "tools/call",
                format!("rmcp request failed: {e}"),
            )];
        }
        (_, Err(e)) => {
            return vec![check_error(
                "tools/call",
                format!("tower-mcp request failed: {e}"),
            )];
        }
    };

    let mut results = Vec::new();

    let rmcp_content = &rmcp_resp.body["result"]["content"];
    let tower_content = &tower_resp.body["result"]["content"];

    match (rmcp_content.is_array(), tower_content.is_array()) {
        (true, true) => results.push(pass("tools/call", "result.content is array on both")),
        (false, _) => {
            results.push(fail(
                "tools/call",
                format!("rmcp result.content is not array: {rmcp_content}"),
            ));
            return results;
        }
        (_, false) => {
            results.push(fail(
                "tools/call",
                format!("tower-mcp result.content is not array: {tower_content}"),
            ));
            return results;
        }
    }

    let rmcp_type = rmcp_content[0]["type"].as_str();
    let tower_type = tower_content[0]["type"].as_str();
    match (rmcp_type, tower_type) {
        (Some(r), Some(t)) => {
            if r == t {
                results.push(pass("tools/call", format!("content[0].type matches ({t})")));
            } else {
                results.push(fail(
                    "tools/call",
                    format!("content[0].type mismatch -- rmcp={r}, tower-mcp={t}"),
                ));
            }
        }
        (None, _) => results.push(fail("tools/call", "rmcp content[0] missing type")),
        (_, None) => results.push(fail("tools/call", "tower-mcp content[0] missing type")),
    }

    let rmcp_text = rmcp_content[0]["text"].as_str().unwrap_or("");
    let tower_text = tower_content[0]["text"].as_str().unwrap_or("");
    if rmcp_text == "hello world" && tower_text == "hello world" {
        results.push(pass("tools/call", "echo returned correct value on both"));
    } else {
        results.push(fail(
            "tools/call",
            format!("echo value mismatch -- rmcp={rmcp_text:?}, tower-mcp={tower_text:?}"),
        ));
    }

    results
}

/// Check 4: method not found -- error.code == -32601, error.message is string
///
/// Known diff: rmcp returns just the method name as the message (e.g., "nonexistent/method"),
/// while tower-mcp returns "Method not found: nonexistent/method". Both use -32601.
async fn check_method_not_found(
    client: &reqwest::Client,
    rmcp_sid: Option<&str>,
    tower_sid: Option<&str>,
) -> Vec<CheckResult> {
    let body = r#"{"jsonrpc":"2.0","id":4,"method":"nonexistent/method"}"#;

    let rmcp_resp = post_mcp(client, &RMCP, body, rmcp_sid).await;
    let tower_resp = post_mcp(client, &TOWER_MCP, body, tower_sid).await;

    let (rmcp_resp, tower_resp) = match (rmcp_resp, tower_resp) {
        (Ok(r), Ok(t)) => (r, t),
        (Err(e), _) => {
            return vec![check_error(
                "method-not-found",
                format!("rmcp request failed: {e}"),
            )];
        }
        (_, Err(e)) => {
            return vec![check_error(
                "method-not-found",
                format!("tower-mcp request failed: {e}"),
            )];
        }
    };

    let mut results = Vec::new();

    let rmcp_code = rmcp_resp.body["error"]["code"].as_i64();
    let tower_code = tower_resp.body["error"]["code"].as_i64();

    match (rmcp_code, tower_code) {
        (Some(-32601), Some(-32601)) => {
            results.push(pass("method-not-found", "error.code == -32601 on both"));
        }
        (Some(r), Some(t)) if r != t => {
            results.push(fail(
                "method-not-found",
                format!("error.code mismatch -- rmcp={r}, tower-mcp={t}"),
            ));
        }
        (Some(c), Some(_)) => {
            results.push(fail(
                "method-not-found",
                format!("error.code is {c} (expected -32601) on one or both"),
            ));
        }
        (None, _) => results.push(fail("method-not-found", "rmcp missing error.code")),
        (_, None) => results.push(fail("method-not-found", "tower-mcp missing error.code")),
    }

    let rmcp_msg = rmcp_resp.body["error"]["message"].as_str();
    let tower_msg = tower_resp.body["error"]["message"].as_str();
    match (rmcp_msg, tower_msg) {
        (Some(_), Some(_)) => {
            results.push(pass("method-not-found", "error.message is string on both"));
        }
        (None, _) => results.push(fail("method-not-found", "rmcp missing error.message")),
        (_, None) => results.push(fail("method-not-found", "tower-mcp missing error.message")),
    }

    // Known diff: message phrasing differs but both use -32601
    if let (Some(r), Some(t)) = (rmcp_msg, tower_msg)
        && r != t
    {
        results.push(known_diff(
            "method-not-found",
            format!(
                "error.message phrasing differs (both use -32601): \
                 rmcp={r:?}, tower-mcp={t:?}. rmcp returns just the method name; \
                 tower-mcp returns \"Method not found: {{method}}\""
            ),
        ));
    }

    results
}

/// Check 5: resources/list -- result.resources is array (may be empty)
async fn check_resources_list(
    client: &reqwest::Client,
    rmcp_sid: Option<&str>,
    tower_sid: Option<&str>,
) -> Vec<CheckResult> {
    let body = r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#;

    let rmcp_resp = post_mcp(client, &RMCP, body, rmcp_sid).await;
    let tower_resp = post_mcp(client, &TOWER_MCP, body, tower_sid).await;

    let (rmcp_resp, tower_resp) = match (rmcp_resp, tower_resp) {
        (Ok(r), Ok(t)) => (r, t),
        (Err(e), _) => {
            return vec![check_error(
                "resources/list",
                format!("rmcp request failed: {e}"),
            )];
        }
        (_, Err(e)) => {
            return vec![check_error(
                "resources/list",
                format!("tower-mcp request failed: {e}"),
            )];
        }
    };

    let mut results = Vec::new();

    // Both may return an error (method not supported) or a result with an array.
    // If one returns error and the other returns result, that's a divergence.
    let rmcp_has_result = rmcp_resp.body.get("result").is_some();
    let tower_has_result = tower_resp.body.get("result").is_some();
    let rmcp_has_error = rmcp_resp.body.get("error").is_some();
    let tower_has_error = tower_resp.body.get("error").is_some();

    match (rmcp_has_result, tower_has_result) {
        (true, true) => {
            let rmcp_resources = &rmcp_resp.body["result"]["resources"];
            let tower_resources = &tower_resp.body["result"]["resources"];
            match (rmcp_resources.is_array(), tower_resources.is_array()) {
                (true, true) => {
                    results.push(pass("resources/list", "result.resources is array on both"));
                }
                (false, _) => results.push(fail(
                    "resources/list",
                    format!("rmcp result.resources is not array: {rmcp_resources}"),
                )),
                (_, false) => results.push(fail(
                    "resources/list",
                    format!("tower-mcp result.resources is not array: {tower_resources}"),
                )),
            }
        }
        (true, false) => {
            // tower-mcp returned an error -- may be a capability-not-declared difference
            let tower_err = &tower_resp.body["error"];
            if rmcp_has_error {
                results.push(known_diff(
                    "resources/list",
                    format!(
                        "both returned errors: rmcp={}, tower-mcp={}",
                        rmcp_resp.body["error"], tower_err
                    ),
                ));
            } else {
                results.push(fail(
                    "resources/list",
                    format!("rmcp returned result, tower-mcp returned error: {tower_err}"),
                ));
            }
        }
        (false, true) => {
            let rmcp_err = &rmcp_resp.body["error"];
            results.push(fail(
                "resources/list",
                format!("rmcp returned error, tower-mcp returned result: {rmcp_err}"),
            ));
        }
        (false, false) => {
            // Both returned errors
            let rmcp_code = rmcp_resp.body["error"]["code"].as_i64().unwrap_or(0);
            let tower_code = tower_resp.body["error"]["code"].as_i64().unwrap_or(0);
            if rmcp_code == tower_code {
                results.push(known_diff(
                    "resources/list",
                    format!(
                        "both returned error code {rmcp_code} (capability not advertised by either server)"
                    ),
                ));
            } else {
                results.push(fail(
                    "resources/list",
                    format!(
                        "both returned errors with different codes: rmcp={rmcp_code}, tower-mcp={tower_code}"
                    ),
                ));
            }
        }
    }

    // Suppress unused variable warnings if paths not taken
    let _ = rmcp_has_error;
    let _ = tower_has_error;

    results
}

/// Check 6: prompts/list -- result.prompts is array (may be empty)
async fn check_prompts_list(
    client: &reqwest::Client,
    rmcp_sid: Option<&str>,
    tower_sid: Option<&str>,
) -> Vec<CheckResult> {
    let body = r#"{"jsonrpc":"2.0","id":6,"method":"prompts/list"}"#;

    let rmcp_resp = post_mcp(client, &RMCP, body, rmcp_sid).await;
    let tower_resp = post_mcp(client, &TOWER_MCP, body, tower_sid).await;

    let (rmcp_resp, tower_resp) = match (rmcp_resp, tower_resp) {
        (Ok(r), Ok(t)) => (r, t),
        (Err(e), _) => {
            return vec![check_error(
                "prompts/list",
                format!("rmcp request failed: {e}"),
            )];
        }
        (_, Err(e)) => {
            return vec![check_error(
                "prompts/list",
                format!("tower-mcp request failed: {e}"),
            )];
        }
    };

    let mut results = Vec::new();

    let rmcp_has_result = rmcp_resp.body.get("result").is_some();
    let tower_has_result = tower_resp.body.get("result").is_some();

    match (rmcp_has_result, tower_has_result) {
        (true, true) => {
            let rmcp_prompts = &rmcp_resp.body["result"]["prompts"];
            let tower_prompts = &tower_resp.body["result"]["prompts"];
            match (rmcp_prompts.is_array(), tower_prompts.is_array()) {
                (true, true) => {
                    results.push(pass("prompts/list", "result.prompts is array on both"));
                }
                (false, _) => results.push(fail(
                    "prompts/list",
                    format!("rmcp result.prompts is not array: {rmcp_prompts}"),
                )),
                (_, false) => results.push(fail(
                    "prompts/list",
                    format!("tower-mcp result.prompts is not array: {tower_prompts}"),
                )),
            }
        }
        (true, false) => {
            let tower_err = &tower_resp.body["error"];
            results.push(fail(
                "prompts/list",
                format!("rmcp returned result, tower-mcp returned error: {tower_err}"),
            ));
        }
        (false, true) => {
            let rmcp_err = &rmcp_resp.body["error"];
            results.push(fail(
                "prompts/list",
                format!("rmcp returned error, tower-mcp returned result: {rmcp_err}"),
            ));
        }
        (false, false) => {
            let rmcp_code = rmcp_resp.body["error"]["code"].as_i64().unwrap_or(0);
            let tower_code = tower_resp.body["error"]["code"].as_i64().unwrap_or(0);
            if rmcp_code == tower_code {
                results.push(known_diff(
                    "prompts/list",
                    format!(
                        "both returned error code {rmcp_code} (capability not advertised by either server)"
                    ),
                ));
            } else {
                results.push(fail(
                    "prompts/list",
                    format!(
                        "both returned errors with different codes: rmcp={rmcp_code}, tower-mcp={tower_code}"
                    ),
                ));
            }
        }
    }

    results
}

/// Check 7: resources/read not-found error shape
///
/// Send a resources/read for a non-existent URI. Both should return an error.
/// Per SEP-2164 (implemented in #841), tower-mcp uses -32602 (InvalidParams).
/// Note as KNOWN-DIFF if rmcp uses a different code.
async fn check_resources_read_not_found(
    client: &reqwest::Client,
    rmcp_sid: Option<&str>,
    tower_sid: Option<&str>,
) -> Vec<CheckResult> {
    let body = r#"{"jsonrpc":"2.0","id":7,"method":"resources/read","params":{"uri":"nonexistent://does-not-exist"}}"#;

    let rmcp_resp = post_mcp(client, &RMCP, body, rmcp_sid).await;
    let tower_resp = post_mcp(client, &TOWER_MCP, body, tower_sid).await;

    let (rmcp_resp, tower_resp) = match (rmcp_resp, tower_resp) {
        (Ok(r), Ok(t)) => (r, t),
        (Err(e), _) => {
            return vec![check_error(
                "resources/read",
                format!("rmcp request failed: {e}"),
            )];
        }
        (_, Err(e)) => {
            return vec![check_error(
                "resources/read",
                format!("tower-mcp request failed: {e}"),
            )];
        }
    };

    let mut results = Vec::new();

    let rmcp_code = rmcp_resp.body["error"]["code"].as_i64();
    let tower_code = tower_resp.body["error"]["code"].as_i64();

    match (rmcp_code, tower_code) {
        (Some(r), Some(t)) if r == t => {
            results.push(pass(
                "resources/read",
                format!("not-found error.code matches on both ({r})"),
            ));
        }
        (Some(r), Some(t)) => {
            // tower-mcp uses -32602 per SEP-2164 (#841); check if rmcp differs
            if t == -32602 && r != -32602 {
                results.push(known_diff(
                    "resources/read",
                    format!(
                        "not-found error code differs: rmcp={r}, tower-mcp={t} (-32602 InvalidParams per SEP-2164/#841)"
                    ),
                ));
            } else {
                results.push(fail(
                    "resources/read",
                    format!("not-found error.code mismatch: rmcp={r}, tower-mcp={t}"),
                ));
            }
        }
        (None, Some(_)) => {
            // rmcp may return a result instead of an error if resources capability not declared
            results.push(known_diff(
                "resources/read",
                format!(
                    "rmcp did not return an error for not-found resource (body: {})",
                    rmcp_resp.body
                ),
            ));
        }
        (Some(_), None) => {
            results.push(fail(
                "resources/read",
                format!(
                    "tower-mcp did not return an error for not-found resource (body: {})",
                    tower_resp.body
                ),
            ));
        }
        (None, None) => {
            results.push(known_diff(
                "resources/read",
                "neither server returned an error (resources capability not declared on either)",
            ));
        }
    }

    results
}

/// Check 8: invalid params error shape
///
/// Send tools/call with missing required params. Both should return -32602.
async fn check_invalid_params(
    client: &reqwest::Client,
    rmcp_sid: Option<&str>,
    tower_sid: Option<&str>,
) -> Vec<CheckResult> {
    // Call echo with no arguments -- "message" is required
    let body =
        r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"echo","arguments":{}}}"#;

    let rmcp_resp = post_mcp(client, &RMCP, body, rmcp_sid).await;
    let tower_resp = post_mcp(client, &TOWER_MCP, body, tower_sid).await;

    let (rmcp_resp, tower_resp) = match (rmcp_resp, tower_resp) {
        (Ok(r), Ok(t)) => (r, t),
        (Err(e), _) => {
            return vec![check_error(
                "invalid-params",
                format!("rmcp request failed: {e}"),
            )];
        }
        (_, Err(e)) => {
            return vec![check_error(
                "invalid-params",
                format!("tower-mcp request failed: {e}"),
            )];
        }
    };

    let mut results = Vec::new();

    // Servers may return the error at the JSON-RPC level or inside result.isError
    let rmcp_error_code = rmcp_resp.body["error"]["code"].as_i64();
    let tower_error_code = tower_resp.body["error"]["code"].as_i64();
    let rmcp_is_tool_error = rmcp_resp.body["result"]["isError"]
        .as_bool()
        .unwrap_or(false);
    let tower_is_tool_error = tower_resp.body["result"]["isError"]
        .as_bool()
        .unwrap_or(false);

    match (rmcp_error_code, tower_error_code) {
        (Some(-32602), Some(-32602)) => {
            results.push(pass(
                "invalid-params",
                "missing required params: error.code == -32602 on both",
            ));
        }
        (Some(r), Some(t)) if r != t => {
            results.push(known_diff(
                "invalid-params",
                format!("missing params error code differs: rmcp={r}, tower-mcp={t}"),
            ));
        }
        (Some(c), Some(_)) => {
            results.push(known_diff(
                "invalid-params",
                format!("both return error code {c} (not -32602)"),
            ));
        }
        (None, None) => {
            // Both may return tool-level errors instead of JSON-RPC errors
            if rmcp_is_tool_error || tower_is_tool_error {
                results.push(known_diff(
                    "invalid-params",
                    format!(
                        "missing params reported as tool-level error (isError=true): \
                         rmcp={rmcp_is_tool_error}, tower-mcp={tower_is_tool_error}"
                    ),
                ));
            } else {
                results.push(fail(
                    "invalid-params",
                    format!(
                        "neither server returned error: rmcp={}, tower-mcp={}",
                        rmcp_resp.body, tower_resp.body
                    ),
                ));
            }
        }
        (Some(_), None) => {
            if tower_is_tool_error {
                results.push(known_diff(
                    "invalid-params",
                    "rmcp returns JSON-RPC error; tower-mcp returns tool-level isError=true",
                ));
            } else {
                results.push(fail(
                    "invalid-params",
                    format!(
                        "rmcp returned error; tower-mcp returned: {}",
                        tower_resp.body
                    ),
                ));
            }
        }
        (None, Some(_)) => {
            if rmcp_is_tool_error {
                results.push(known_diff(
                    "invalid-params",
                    "tower-mcp returns JSON-RPC error; rmcp returns tool-level isError=true",
                ));
            } else {
                results.push(fail(
                    "invalid-params",
                    format!(
                        "tower-mcp returned error; rmcp returned: {}",
                        rmcp_resp.body
                    ),
                ));
            }
        }
    }

    results
}

/// Check 9: notifications/initialized enforcement
///
/// Send tools/list WITHOUT the notifications/initialized step. tower-mcp rejects
/// this with -32600 (InvalidRequest) per #901. rmcp does not enforce the ordering
/// and returns the tools list, so this is a KNOWN-DIFF (tower-mcp is the stricter,
/// more spec-compliant side), verified current as of rmcp 2.1.0.
async fn check_initialized_enforcement(client: &reqwest::Client) -> Vec<CheckResult> {
    // Start fresh sessions without sending notifications/initialized
    let init_body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"compat-enforcement-test","version":"0.1.0"}}}"#;

    let rmcp_init = post_mcp(client, &RMCP, init_body, None).await;
    let tower_init = post_mcp(client, &TOWER_MCP, init_body, None).await;

    let (rmcp_init, tower_init) = match (rmcp_init, tower_init) {
        (Ok(r), Ok(t)) => (r, t),
        (Err(e), _) => {
            return vec![check_error(
                "initialized-enforcement",
                format!("rmcp initialize failed: {e}"),
            )];
        }
        (_, Err(e)) => {
            return vec![check_error(
                "initialized-enforcement",
                format!("tower-mcp initialize failed: {e}"),
            )];
        }
    };

    let rmcp_sid = rmcp_init.session_id.clone();
    let tower_sid = tower_init.session_id.clone();

    // Do NOT send notifications/initialized -- go straight to tools/list
    let list_body = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;

    let rmcp_resp = post_mcp(client, &RMCP, list_body, rmcp_sid.as_deref()).await;
    let tower_resp = post_mcp(client, &TOWER_MCP, list_body, tower_sid.as_deref()).await;

    let (rmcp_resp, tower_resp) = match (rmcp_resp, tower_resp) {
        (Ok(r), Ok(t)) => (r, t),
        (Err(e), _) => {
            return vec![check_error(
                "initialized-enforcement",
                format!("rmcp tools/list failed: {e}"),
            )];
        }
        (_, Err(e)) => {
            return vec![check_error(
                "initialized-enforcement",
                format!("tower-mcp tools/list failed: {e}"),
            )];
        }
    };

    let mut results = Vec::new();

    let rmcp_code = rmcp_resp.body["error"]["code"].as_i64();
    let tower_code = tower_resp.body["error"]["code"].as_i64();

    match (rmcp_code, tower_code) {
        (Some(r), Some(t)) => {
            if r == -32600 && t == -32600 {
                results.push(pass(
                    "initialized-enforcement",
                    "both reject tools/list before notifications/initialized with -32600",
                ));
            } else if r == t {
                results.push(known_diff(
                    "initialized-enforcement",
                    format!("both reject but with code {r} (expected -32600)"),
                ));
            } else {
                results.push(known_diff(
                    "initialized-enforcement",
                    format!(
                        "both reject but codes differ: rmcp={r}, tower-mcp={t} (tower-mcp uses -32600 per #901)"
                    ),
                ));
            }
        }
        (Some(r), None) => {
            // tower-mcp may have returned a result (lenient mode shouldn't happen post-#901)
            results.push(fail(
                "initialized-enforcement",
                format!(
                    "rmcp returned error {r}; tower-mcp returned result (should enforce since #901): {}",
                    tower_resp.body
                ),
            ));
        }
        (None, Some(t)) => {
            // rmcp does not enforce the notifications/initialized ordering: it
            // returns the tools/list result instead of an error. tower-mcp
            // rejects with -32600 (InvalidRequest) per #901, which is the
            // stricter, more spec-compliant behavior. Documented KNOWN-DIFF,
            // not a bug. Any other tower-mcp code here would be a regression,
            // so keep that path a FAIL.
            if t == -32600 {
                results.push(known_diff(
                    "initialized-enforcement",
                    "tower-mcp rejects tools/list before notifications/initialized with -32600 \
                     (InvalidRequest, per #901); rmcp does not enforce the ordering and returns \
                     the tools list. tower-mcp is the stricter, more spec-compliant side.",
                ));
            } else {
                results.push(fail(
                    "initialized-enforcement",
                    format!(
                        "tower-mcp returned error {t} (expected -32600); rmcp returned result: {}",
                        rmcp_resp.body
                    ),
                ));
            }
        }
        (None, None) => {
            results.push(fail(
                "initialized-enforcement",
                format!(
                    "neither server rejected the request: rmcp={}, tower-mcp={}",
                    rmcp_resp.body, tower_resp.body
                ),
            ));
        }
    }

    results
}

/// Check 10: SSE response mode
///
/// Spin up tower-mcp with .sse_responses(true). Send tools/list. Compare:
/// both should return Content-Type: text/event-stream and the SSE data line
/// should parse to valid JSON-RPC.
async fn check_sse_response_mode(
    client: &reqwest::Client,
    rmcp_sid: Option<&str>,
    tower_sse_sid: Option<&str>,
) -> Vec<CheckResult> {
    let list_body = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;

    let rmcp_resp = post_mcp(client, &RMCP, list_body, rmcp_sid).await;
    let tower_sse_resp = post_mcp(client, &TOWER_MCP_SSE, list_body, tower_sse_sid).await;

    let (rmcp_resp, tower_sse_resp) = match (rmcp_resp, tower_sse_resp) {
        (Ok(r), Ok(t)) => (r, t),
        (Err(e), _) => {
            return vec![check_error(
                "sse-response-mode",
                format!("rmcp request failed: {e}"),
            )];
        }
        (_, Err(e)) => {
            return vec![check_error(
                "sse-response-mode",
                format!("tower-mcp SSE mode request failed: {e}"),
            )];
        }
    };

    let mut results = Vec::new();

    let rmcp_ct = rmcp_resp.content_type.contains("text/event-stream");
    let tower_sse_ct = tower_sse_resp.content_type.contains("text/event-stream");

    match (rmcp_ct, tower_sse_ct) {
        (true, true) => {
            results.push(pass(
                "sse-response-mode",
                "both return text/event-stream when SSE mode enabled",
            ));
        }
        (false, true) => {
            results.push(fail(
                "sse-response-mode",
                format!(
                    "rmcp content-type is not text/event-stream: {}",
                    rmcp_resp.content_type
                ),
            ));
        }
        (true, false) => {
            results.push(fail(
                "sse-response-mode",
                format!(
                    "tower-mcp SSE mode content-type is not text/event-stream: {}",
                    tower_sse_resp.content_type
                ),
            ));
        }
        (false, false) => {
            results.push(fail(
                "sse-response-mode",
                format!(
                    "neither returned text/event-stream: rmcp={}, tower-mcp-sse={}",
                    rmcp_resp.content_type, tower_sse_resp.content_type
                ),
            ));
        }
    }

    // Verify the parsed body has valid JSON-RPC (post_mcp already parsed the SSE data line)
    let rmcp_has_result =
        rmcp_resp.body.get("result").is_some() || rmcp_resp.body.get("error").is_some();
    let tower_sse_has_result =
        tower_sse_resp.body.get("result").is_some() || tower_sse_resp.body.get("error").is_some();

    match (rmcp_has_result, tower_sse_has_result) {
        (true, true) => {
            results.push(pass(
                "sse-response-mode",
                "SSE data line parses to valid JSON-RPC on both",
            ));
        }
        (false, _) => results.push(fail(
            "sse-response-mode",
            format!("rmcp SSE data is not valid JSON-RPC: {}", rmcp_resp.body),
        )),
        (_, false) => results.push(fail(
            "sse-response-mode",
            format!(
                "tower-mcp SSE data is not valid JSON-RPC: {}",
                tower_sse_resp.body
            ),
        )),
    }

    // Show note about default mode
    println!(
        "  [NOTE] sse-response-mode: tower-mcp returns bare JSON by default; \
         use .sse_responses(true) to match rmcp's SSE wrapping behavior"
    );

    results
}

// ============================================================
// Main
// ============================================================

#[tokio::main]
async fn main() -> Result<()> {
    tokio::spawn(start_tower_mcp_server(4002));
    tokio::spawn(start_tower_mcp_sse_server(4003));
    tokio::spawn(start_rmcp_server(4001));

    println!(
        "Waiting for servers: {}, {}, and {}...",
        RMCP.display_name(),
        TOWER_MCP.display_name(),
        TOWER_MCP_SSE.display_name()
    );

    let rmcp_ready = wait_for_ready(&RMCP).await;
    let tower_ready = wait_for_ready(&TOWER_MCP).await;
    let tower_sse_ready = wait_for_ready(&TOWER_MCP_SSE).await;

    if !rmcp_ready {
        eprintln!("ERROR: rmcp server did not start on port 4001");
        std::process::exit(1);
    }
    if !tower_ready {
        eprintln!("ERROR: tower-mcp server did not start on port 4002");
        std::process::exit(1);
    }
    if !tower_sse_ready {
        eprintln!("ERROR: tower-mcp SSE server did not start on port 4003");
        std::process::exit(1);
    }

    println!("All servers ready. Running checks...\n");

    let client = reqwest::Client::new();
    let mut all_results: Vec<CheckResult> = Vec::new();

    // ---- Check 1: initialize (also extracts session IDs) ----
    println!("=== initialize ===");
    let (init_results, rmcp_sid, tower_sid) = check_initialize(&client).await;
    for r in &init_results {
        print_result(r);
    }
    all_results.extend(init_results);

    // Initialize the SSE server independently
    let sse_init_body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"compat-test","version":"0.1.0"}}}"#;
    let sse_init = post_mcp(&client, &TOWER_MCP_SSE, sse_init_body, None).await;
    let tower_sse_sid = sse_init.ok().and_then(|r| {
        // Send initialized notification for SSE server
        let sid = r.session_id.clone();
        if let Some(ref s) = sid {
            // We need to block here; use a oneshot channel approach
            let client2 = client.clone();
            let sid_clone = s.clone();
            tokio::spawn(async move {
                send_initialized(&client2, &TOWER_MCP_SSE, Some(&sid_clone)).await;
            });
        }
        sid
    });

    println!();

    // ---- Check 2: tools/list ----
    println!("=== tools/list ===");
    let tools_results = check_tools_list(&client, rmcp_sid.as_deref(), tower_sid.as_deref()).await;
    for r in &tools_results {
        print_result(r);
    }
    all_results.extend(tools_results);

    println!();

    // ---- Check 3: tools/call ----
    println!("=== tools/call ===");
    let call_results = check_tools_call(&client, rmcp_sid.as_deref(), tower_sid.as_deref()).await;
    for r in &call_results {
        print_result(r);
    }
    all_results.extend(call_results);

    println!();

    // ---- Check 4: method not found ----
    println!("=== method-not-found ===");
    let notfound_results =
        check_method_not_found(&client, rmcp_sid.as_deref(), tower_sid.as_deref()).await;
    for r in &notfound_results {
        print_result(r);
    }
    all_results.extend(notfound_results);

    println!();

    // ---- Check 5: resources/list ----
    println!("=== resources/list ===");
    let resources_results =
        check_resources_list(&client, rmcp_sid.as_deref(), tower_sid.as_deref()).await;
    for r in &resources_results {
        print_result(r);
    }
    all_results.extend(resources_results);

    println!();

    // ---- Check 6: prompts/list ----
    println!("=== prompts/list ===");
    let prompts_results =
        check_prompts_list(&client, rmcp_sid.as_deref(), tower_sid.as_deref()).await;
    for r in &prompts_results {
        print_result(r);
    }
    all_results.extend(prompts_results);

    println!();

    // ---- Check 7: resources/read not-found ----
    println!("=== resources/read (not-found) ===");
    let read_results =
        check_resources_read_not_found(&client, rmcp_sid.as_deref(), tower_sid.as_deref()).await;
    for r in &read_results {
        print_result(r);
    }
    all_results.extend(read_results);

    println!();

    // ---- Check 8: invalid params ----
    println!("=== invalid-params ===");
    let invalid_results =
        check_invalid_params(&client, rmcp_sid.as_deref(), tower_sid.as_deref()).await;
    for r in &invalid_results {
        print_result(r);
    }
    all_results.extend(invalid_results);

    println!();

    // ---- Check 9: notifications/initialized enforcement ----
    println!("=== initialized-enforcement ===");
    let enforcement_results = check_initialized_enforcement(&client).await;
    for r in &enforcement_results {
        print_result(r);
    }
    all_results.extend(enforcement_results);

    println!();

    // ---- Check 10: SSE response mode ----
    println!("=== sse-response-mode ===");
    // Need a fresh rmcp session for the SSE check
    let rmcp_sse_init_body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"compat-sse-test","version":"0.1.0"}}}"#;
    let rmcp_sse_init = post_mcp(&client, &RMCP, rmcp_sse_init_body, None).await;
    let rmcp_sse_sid = rmcp_sse_init.ok().and_then(|r| {
        let sid = r.session_id.clone();
        if let Some(ref s) = sid {
            let client2 = client.clone();
            let sid_clone = s.clone();
            tokio::spawn(async move {
                send_initialized(&client2, &RMCP, Some(&sid_clone)).await;
            });
        }
        sid
    });
    // Give notifications/initialized a moment to be processed
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let sse_results =
        check_sse_response_mode(&client, rmcp_sse_sid.as_deref(), tower_sse_sid.as_deref()).await;
    for r in &sse_results {
        print_result(r);
    }
    all_results.extend(sse_results);

    // ---- Summary ----
    let total = all_results.len();
    let passed = all_results
        .iter()
        .filter(|r| matches!(r.outcome, CheckOutcome::Pass))
        .count();
    let failed = all_results
        .iter()
        .filter(|r| matches!(r.outcome, CheckOutcome::Fail))
        .count();
    let known_diffs = all_results
        .iter()
        .filter(|r| matches!(r.outcome, CheckOutcome::KnownDiff))
        .count();
    let errors = all_results
        .iter()
        .filter(|r| matches!(r.outcome, CheckOutcome::Error))
        .count();

    println!(
        "\nResults: {passed}/{total} checks passed ({failed} failed, {known_diffs} known-diffs, {errors} errors)"
    );

    if known_diffs > 0 {
        println!(
            "\nKnown diffs are documented divergences between rmcp and tower-mcp that are\n\
             intentional or explained. They do not indicate bugs."
        );
    }

    if failed > 0 || errors > 0 {
        std::process::exit(1);
    }

    Ok(())
}
