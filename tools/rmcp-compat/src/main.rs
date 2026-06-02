//! Wire-format compatibility spike: rmcp vs tower-mcp
//!
//! Starts both servers in-process as tokio tasks, fires identical MCP JSON-RPC
//! requests at each, and structurally compares the responses.
//!
//! rmcp server: port 4001 (path: /mcp/)
//! tower-mcp server: port 4002 (path: /)
//!
//! Run with:
//!   cargo run -p rmcp-compat
//!
//! This is a spike -- rough edges are intentional.

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

// ============================================================
// tower-mcp server
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
            Ok(CallToolResult::success(vec![Content::text(message)]))
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
// Test harness
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
    // rmcp's StreamableHttpService returns SSE-wrapped JSON even for
    // synchronous RPC responses. Parse the SSE envelope and extract
    // the JSON from a `data:` line.
    let body: Value = if content_type.contains("text/event-stream") {
        let text = String::from_utf8_lossy(&bytes);
        // Extract the first `data:` line from the SSE stream
        text.lines()
            .filter(|line| line.starts_with("data:"))
            .map(|line| line.trim_start_matches("data:").trim())
            .filter(|data| !data.is_empty())
            .filter_map(|data| serde_json::from_str(data).ok())
            .next()
            .ok_or_else(|| anyhow::anyhow!("no valid data: line in SSE body: {text}"))?
    } else {
        serde_json::from_slice(&bytes).map_err(|e| {
            let text = String::from_utf8_lossy(&bytes);
            anyhow::anyhow!("JSON parse error: {e} | body: {text}")
        })?
    };
    Ok(ServerResponse { body, session_id })
}

// ============================================================
// Check result types
// ============================================================

#[derive(Serialize)]
enum CheckOutcome {
    Pass,
    Fail,
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

    // MCP spec requires client to send notifications/initialized before other requests
    // rmcp enforces this strictly; tower-mcp is lenient
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

/// Check 3: tools/call echo -- result.content array, content[0].type
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

    // Show actual messages for documentation -- they will likely differ
    if let (Some(r), Some(t)) = (rmcp_msg, tower_msg)
        && r != t
    {
        println!(
            "  [INFO] error.message content differs (expected) -- rmcp={r:?}, tower-mcp={t:?}"
        );
    }

    results
}

// ============================================================
// Main
// ============================================================

#[tokio::main]
async fn main() -> Result<()> {
    tokio::spawn(start_tower_mcp_server(4002));
    tokio::spawn(start_rmcp_server(4001));

    println!(
        "Waiting for servers: {} and {}...",
        RMCP.display_name(),
        TOWER_MCP.display_name()
    );

    let rmcp_ready = wait_for_ready(&RMCP).await;
    let tower_ready = wait_for_ready(&TOWER_MCP).await;

    if !rmcp_ready {
        eprintln!("ERROR: rmcp server did not start on port 4001");
        std::process::exit(1);
    }
    if !tower_ready {
        eprintln!("ERROR: tower-mcp server did not start on port 4002");
        std::process::exit(1);
    }

    println!("Both servers ready. Running checks...\n");

    let client = reqwest::Client::new();
    let mut all_results: Vec<CheckResult> = Vec::new();

    // Check 1: initialize (also extracts session IDs)
    let (init_results, rmcp_sid, tower_sid) = check_initialize(&client).await;
    for r in &init_results {
        print_result(r);
    }
    all_results.extend(init_results);

    println!();

    // Check 2: tools/list
    let tools_results = check_tools_list(&client, rmcp_sid.as_deref(), tower_sid.as_deref()).await;
    for r in &tools_results {
        print_result(r);
    }
    all_results.extend(tools_results);

    println!();

    // Check 3: tools/call
    let call_results = check_tools_call(&client, rmcp_sid.as_deref(), tower_sid.as_deref()).await;
    for r in &call_results {
        print_result(r);
    }
    all_results.extend(call_results);

    println!();

    // Check 4: method not found
    let notfound_results =
        check_method_not_found(&client, rmcp_sid.as_deref(), tower_sid.as_deref()).await;
    for r in &notfound_results {
        print_result(r);
    }
    all_results.extend(notfound_results);

    let total = all_results.len();
    let passed = all_results
        .iter()
        .filter(|r| matches!(r.outcome, CheckOutcome::Pass))
        .count();
    let failed = all_results
        .iter()
        .filter(|r| matches!(r.outcome, CheckOutcome::Fail))
        .count();
    let errors = all_results
        .iter()
        .filter(|r| matches!(r.outcome, CheckOutcome::Error))
        .count();

    println!("\nResults: {passed}/{total} checks passed ({failed} failed, {errors} errors)");

    if failed > 0 || errors > 0 {
        std::process::exit(1);
    }

    Ok(())
}
