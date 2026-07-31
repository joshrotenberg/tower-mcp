//! Wire-format compatibility harness: rmcp vs tower-mcp
//!
//! Starts both servers in-process as tokio tasks, fires identical MCP JSON-RPC
//! requests at each, and structurally compares the responses.
//!
//! Each server binds an OS-assigned loopback port so concurrent local and CI
//! runs cannot collide.
//!
//! Run with:
//!   cargo run -p rmcp-compat

use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;
use tower_mcp::{
    CallToolResult, HttpTransport, InputRequiredResult, McpRouter, ProtocolSupport, RequestOutcome,
    TaskSupportMode, ToolBuilder,
};

// ============================================================
// tower-mcp server (standard mode)
// ============================================================

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EchoInput {
    message: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct MrtrInput {}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SlowEchoInput {
    delay_ms: u64,
}

async fn start_tower_mcp_server(listener: tokio::net::TcpListener) {
    let echo = ToolBuilder::new("echo")
        .description("Echo a message back")
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
        .build();

    let router = McpRouter::new()
        .server_info("tower-mcp-compat-test", "0.1.0")
        .tool(echo);

    let transport = HttpTransport::new(router).disable_origin_validation();

    if let Err(e) = axum::serve(listener, transport.into_router()).await {
        eprintln!("tower-mcp server error: {e}");
    }
}

// ============================================================
// tower-mcp server (SSE mode -- mirrors rmcp's SSE wrapping)
// ============================================================

async fn start_tower_mcp_sse_server(listener: tokio::net::TcpListener) {
    let echo = ToolBuilder::new("echo")
        .description("Echo a message back")
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
        .build();

    let router = McpRouter::new()
        .server_info("tower-mcp-compat-test-sse", "0.1.0")
        .tool(echo);

    let transport = HttpTransport::new(router)
        .disable_origin_validation()
        .sse_responses(true);

    if let Err(e) = axum::serve(listener, transport.into_router()).await {
        eprintln!("tower-mcp SSE server error: {e}");
    }
}

// ============================================================
// tower-mcp server (2026-07-28 stateless final protocol)
// ============================================================

async fn start_tower_mcp_final_server(listener: tokio::net::TcpListener) {
    let echo = ToolBuilder::new("echo")
        .description("Echo a message back")
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
        .build();

    let mrtr = ToolBuilder::new("mrtr")
        .description("Require one state-only retry before completing")
        .mrtr_handler(|ctx, _input: MrtrInput| async move {
            if ctx.request_state().is_none() {
                Ok(RequestOutcome::InputRequired(
                    InputRequiredResult::new().with_request_state("compat-mrtr-state"),
                ))
            } else if ctx.request_state() == Some("compat-mrtr-state") {
                Ok(RequestOutcome::Complete(CallToolResult::text(
                    "mrtr complete",
                )))
            } else {
                Err(tower_mcp::Error::invalid_params("unexpected requestState"))
            }
        })
        .build();

    let slow_echo = ToolBuilder::new("slow_echo")
        .description("Complete asynchronously when the Tasks extension is negotiated")
        .task_support(TaskSupportMode::Optional)
        .handler(|input: SlowEchoInput| async move {
            tokio::time::sleep(Duration::from_millis(input.delay_ms)).await;
            Ok(CallToolResult::text("task complete"))
        })
        .build();

    let router = McpRouter::new()
        .server_info("tower-mcp-compat-final", "0.1.0")
        .tool(echo)
        .tool(mrtr)
        .tool(slow_echo)
        .with_tasks();

    let transport = HttpTransport::new(router)
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).expect("known protocol"))
        .disable_origin_validation();

    if let Err(e) = axum::serve(listener, transport.into_router()).await {
        eprintln!("tower-mcp final server error: {e}");
    }
}

// ============================================================
// rmcp server
// ============================================================

mod rmcp_server {
    use std::borrow::Cow;

    use rmcp::{
        ErrorData as McpError, ServerHandler,
        handler::server::{router::tool::ToolRouter, wrapper::Parameters},
        model::*,
        service::{RequestContext, RoleServer},
        task_manager::{TaskExit, TaskManager, TaskOptions},
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

    #[derive(Clone)]
    pub struct FinalServer {
        tool_router: ToolRouter<FinalServer>,
        tasks: TaskManager,
    }

    #[tool_router]
    impl FinalServer {
        pub fn new() -> Self {
            Self {
                tool_router: Self::tool_router(),
                tasks: TaskManager::new(),
            }
        }

        #[tool(description = "Echo a message back")]
        fn echo(
            &self,
            Parameters(EchoParams { message }): Parameters<EchoParams>,
        ) -> Result<CallToolResult, McpError> {
            Ok(CallToolResult::success(vec![ContentBlock::text(message)]))
        }

        #[tool(description = "Require one state-only retry before completing")]
        fn mrtr(&self) -> Result<CallToolResult, McpError> {
            // The custom dispatch below preserves InputRequiredResult; this
            // definition exists so tools/list exposes a matching schema.
            Ok(CallToolResult::success(vec![ContentBlock::text(
                "mrtr complete",
            )]))
        }

        #[tool(description = "Complete asynchronously when Tasks is negotiated")]
        fn slow_echo(&self) -> Result<CallToolResult, McpError> {
            // The custom dispatch below materializes the task after reading
            // delay_ms directly from the request arguments.
            Ok(CallToolResult::success(vec![ContentBlock::text(
                "task complete",
            )]))
        }
    }

    impl ServerHandler for FinalServer {
        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, McpError> {
            if request.name == "mrtr" {
                return match request.request_state.as_deref() {
                    None => Ok(InputRequiredResult::from_request_state("compat-mrtr-state").into()),
                    Some("compat-mrtr-state") => {
                        Ok(
                            CallToolResult::success(vec![ContentBlock::text("mrtr complete")])
                                .into(),
                        )
                    }
                    Some(_) => Err(McpError::invalid_params("unexpected requestState", None)),
                };
            }

            if request.name == "slow_echo"
                && context
                    .client_capabilities()
                    .is_some_and(|caps| caps.supports_tasks())
            {
                let delay_ms = request
                    .arguments
                    .as_ref()
                    .and_then(|args| args.get("delay_ms"))
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| McpError::invalid_params("delay_ms is required", None))?;
                let task = self.tasks.spawn(
                    TaskOptions::new().with_poll_interval_ms(10),
                    move |ctx| {
                        Box::pin(async move {
                            tokio::select! {
                                _ = ctx.cancelled() => Err(TaskExit::Cancelled),
                                _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {
                                    Ok(CallToolResult::success(vec![ContentBlock::text("task complete")]))
                                }
                            }
                        })
                    },
                );
                return Ok(CallToolResponse::Task(CreateTaskResult::new(task)));
            }

            let call = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
            self.tool_router.call(call).await
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, McpError> {
            Ok(ListToolsResult {
                tools: self.tool_router.list_all(),
                ..Default::default()
            })
        }

        fn get_tool(&self, name: &str) -> Option<Tool> {
            self.tool_router.get(name).cloned()
        }

        async fn get_task(
            &self,
            request: GetTaskParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<GetTaskResult, McpError> {
            Ok(GetTaskResult::new(self.tasks.get_task(&request.task_id)?))
        }

        async fn update_task(
            &self,
            request: UpdateTaskParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<(), McpError> {
            self.tasks
                .update_task(&request.task_id, request.input_responses)
        }

        async fn cancel_task(
            &self,
            request: CancelTaskParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<(), McpError> {
            self.tasks.cancel_task(&request.task_id)
        }

        fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
            Cow::Owned(vec![ProtocolVersion::V_2026_07_28])
        }

        fn accepted_subscription_filter(
            &self,
            requested: &SubscriptionFilter,
        ) -> Option<SubscriptionFilter> {
            Some(requested.clone())
        }

        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(
                ServerCapabilities::builder()
                    .enable_tools()
                    .enable_tool_list_changed()
                    .enable_tasks()
                    .build(),
            )
            .with_server_info(Implementation::new("rmcp-compat-final", "0.1.0"))
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
        }
    }
}

async fn start_rmcp_server(listener: tokio::net::TcpListener) {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };

    let service = StreamableHttpService::new(
        || Ok(rmcp_server::EchoServer::new()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );

    let axum_router = axum::Router::new().nest_service("/mcp", service);
    if let Err(e) = axum::serve(listener, axum_router).await {
        eprintln!("rmcp server error: {e}");
    }
}

async fn start_rmcp_final_server(listener: tokio::net::TcpListener) {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };

    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_stateless_protocol_metadata_required(true);
    let server = rmcp_server::FinalServer::new();
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        config,
    );

    let axum_router = axum::Router::new().nest_service("/mcp", service);
    if let Err(e) = axum::serve(listener, axum_router).await {
        eprintln!("rmcp final server error: {e}");
    }
}

// ============================================================
// Server configuration
// ============================================================

struct ServerConfig {
    name: &'static str,
    port: &'static AtomicU16,
    path: &'static str,
}

impl ServerConfig {
    fn port(&self) -> u16 {
        self.port.load(Ordering::Relaxed)
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port(), self.path)
    }

    fn display_name(&self) -> String {
        format!("{} (port {})", self.name, self.port())
    }
}

static RMCP_PORT: AtomicU16 = AtomicU16::new(0);
static TOWER_MCP_PORT: AtomicU16 = AtomicU16::new(0);
static TOWER_MCP_SSE_PORT: AtomicU16 = AtomicU16::new(0);
static RMCP_FINAL_PORT: AtomicU16 = AtomicU16::new(0);
static TOWER_MCP_FINAL_PORT: AtomicU16 = AtomicU16::new(0);

const RMCP: ServerConfig = ServerConfig {
    name: "rmcp",
    port: &RMCP_PORT,
    path: "/mcp/",
};

const TOWER_MCP: ServerConfig = ServerConfig {
    name: "tower-mcp",
    port: &TOWER_MCP_PORT,
    path: "/",
};

const TOWER_MCP_SSE: ServerConfig = ServerConfig {
    name: "tower-mcp-sse",
    port: &TOWER_MCP_SSE_PORT,
    path: "/",
};

const RMCP_FINAL: ServerConfig = ServerConfig {
    name: "rmcp-final",
    port: &RMCP_FINAL_PORT,
    path: "/mcp/",
};

const TOWER_MCP_FINAL: ServerConfig = ServerConfig {
    name: "tower-mcp-final",
    port: &TOWER_MCP_FINAL_PORT,
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
    status: u16,
}

async fn post_mcp(
    client: &reqwest::Client,
    server: &ServerConfig,
    body: &str,
    session_id: Option<&str>,
) -> Result<ServerResponse> {
    post_mcp_with_headers(client, server, body, session_id, &[]).await
}

async fn post_mcp_with_headers(
    client: &reqwest::Client,
    server: &ServerConfig,
    body: &str,
    session_id: Option<&str>,
    headers: &[(&str, &str)],
) -> Result<ServerResponse> {
    let mut req = client
        .post(server.url())
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body.to_string());

    if let Some(sid) = session_id {
        req = req.header("mcp-session-id", sid);
    }
    for (name, value) in headers {
        req = req.header(*name, *value);
    }

    let resp = req.send().await?;
    let status = resp.status().as_u16();
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
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({ "raw": raw_body }))
    };
    Ok(ServerResponse {
        body,
        session_id,
        content_type,
        status,
    })
}

fn final_meta(tasks: bool) -> Value {
    let extensions = if tasks {
        json!({ "io.modelcontextprotocol/tasks": {} })
    } else {
        json!({})
    };
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {
            "name": "rmcp-compat-final-client",
            "version": "0.1.0"
        },
        "io.modelcontextprotocol/clientCapabilities": {
            "extensions": extensions
        }
    })
}

fn final_body(id: Value, method: &str, mut params: Value, tasks: bool) -> String {
    params
        .as_object_mut()
        .expect("final request params must be an object")
        .insert("_meta".to_string(), final_meta(tasks));
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    })
    .to_string()
}

async fn post_final(
    client: &reqwest::Client,
    server: &ServerConfig,
    method: &str,
    name: Option<&str>,
    body: &str,
) -> Result<ServerResponse> {
    let mut headers = vec![
        ("mcp-protocol-version", "2026-07-28"),
        ("mcp-method", method),
    ];
    if let Some(name) = name {
        headers.push(("mcp-name", name));
    }
    post_mcp_with_headers(client, server, body, None, &headers).await
}

async fn post_final_stream_first_message(
    client: &reqwest::Client,
    server: &ServerConfig,
    body: &str,
) -> Result<ServerResponse> {
    let mut resp = client
        .post(server.url())
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "subscriptions/listen")
        .body(body.to_string())
        .send()
        .await?;
    let status = resp.status().as_u16();
    let session_id = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let mut buffer = String::new();

    loop {
        let chunk = tokio::time::timeout(Duration::from_secs(3), resp.chunk())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "{} timed out waiting for subscription acknowledgment (HTTP {status}, {content_type})",
                    server.name
                )
            })??
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} subscription stream ended before acknowledgment (HTTP {status}, {content_type}): {buffer}",
                    server.name
                )
            })?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        for line in buffer.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();
                if !data.is_empty()
                    && let Ok(body) = serde_json::from_str::<Value>(data)
                {
                    return Ok(ServerResponse {
                        body,
                        session_id,
                        content_type,
                        status,
                    });
                }
            }
        }
    }
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
/// more spec-compliant side), verified current as of rmcp 3.1.0.
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
// 2026-07-28 final-protocol checks
// ============================================================

async fn final_pair(
    client: &reqwest::Client,
    method: &str,
    name: Option<&str>,
    body: &str,
) -> Result<(ServerResponse, ServerResponse)> {
    let rmcp = post_final(client, &RMCP_FINAL, method, name, body).await?;
    let tower = post_final(client, &TOWER_MCP_FINAL, method, name, body).await?;
    Ok((rmcp, tower))
}

async fn check_final_discover(client: &reqwest::Client) -> Vec<CheckResult> {
    let name = "final-discover";
    let body = final_body(json!(101), "server/discover", json!({}), false);
    let (rmcp, tower) = match final_pair(client, "server/discover", None, &body).await {
        Ok(pair) => pair,
        Err(error) => return vec![check_error(name, error.to_string())],
    };
    let mut results = Vec::new();

    if rmcp.status == 200 && tower.status == 200 {
        results.push(pass(name, "server/discover returns HTTP 200 on both"));
    } else {
        results.push(fail(
            name,
            format!(
                "unexpected status: rmcp={}, tower-mcp={}",
                rmcp.status, tower.status
            ),
        ));
    }
    if rmcp.session_id.is_none() && tower.session_id.is_none() {
        results.push(pass(
            name,
            "neither stateless response creates an MCP session",
        ));
    } else {
        results.push(fail(
            name,
            format!(
                "final response exposed a session: rmcp={:?}, tower-mcp={:?}",
                rmcp.session_id, tower.session_id
            ),
        ));
    }

    let rmcp_versions = rmcp.body["result"]["supportedVersions"].as_array();
    let tower_versions = tower.body["result"]["supportedVersions"].as_array();
    let supports_final = |versions: Option<&Vec<Value>>| {
        versions.is_some_and(|versions| versions.iter().any(|v| v == "2026-07-28"))
    };
    if supports_final(rmcp_versions) && supports_final(tower_versions) {
        results.push(pass(name, "both advertise 2026-07-28"));
    } else {
        results.push(fail(
            name,
            format!(
                "final version missing: rmcp={:?}, tower-mcp={:?}",
                rmcp_versions, tower_versions
            ),
        ));
    }

    let rmcp_result = &rmcp.body["result"];
    let tower_result = &tower.body["result"];
    if rmcp_result["resultType"] == "complete" && tower_result["resultType"] == "complete" {
        results.push(pass(name, "resultType is complete on both"));
    } else {
        results.push(fail(
            name,
            format!(
                "resultType mismatch: rmcp={}, tower-mcp={}",
                rmcp_result["resultType"], tower_result["resultType"]
            ),
        ));
    }
    if rmcp_result["ttlMs"].is_number()
        && tower_result["ttlMs"].is_number()
        && rmcp_result["cacheScope"].is_string()
        && tower_result["cacheScope"].is_string()
    {
        results.push(pass(name, "required cache metadata is present on both"));
    } else {
        results.push(fail(
            name,
            format!(
                "cache metadata mismatch: rmcp ttl/scope={}/{}, tower-mcp ttl/scope={}/{}",
                rmcp_result["ttlMs"],
                rmcp_result["cacheScope"],
                tower_result["ttlMs"],
                tower_result["cacheScope"]
            ),
        ));
    }
    if rmcp_result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"].is_string()
        && tower_result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"].is_string()
        && rmcp_result.get("serverInfo").is_none()
        && tower_result.get("serverInfo").is_none()
    {
        results.push(pass(
            name,
            "server identity lives in result metadata on both",
        ));
    } else {
        results.push(fail(
            name,
            format!("serverInfo placement differs: rmcp={rmcp_result}, tower-mcp={tower_result}"),
        ));
    }
    results
}

async fn check_final_stateless_tools(client: &reqwest::Client) -> Vec<CheckResult> {
    let name = "final-stateless-tools";
    let list_body = final_body(json!(102), "tools/list", json!({}), false);
    let first = final_pair(client, "tools/list", None, &list_body).await;
    let second = final_pair(client, "tools/list", None, &list_body).await;
    let ((rmcp_first, tower_first), (rmcp_second, tower_second)) = match (first, second) {
        (Ok(first), Ok(second)) => (first, second),
        (Err(error), _) | (_, Err(error)) => return vec![check_error(name, error.to_string())],
    };
    let mut results = Vec::new();
    let has_echo = |response: &ServerResponse| {
        response.body["result"]["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "echo"))
    };
    if [&rmcp_first, &tower_first, &rmcp_second, &tower_second]
        .into_iter()
        .all(has_echo)
    {
        results.push(pass(
            name,
            "independent tools/list requests succeed on both",
        ));
    } else {
        results.push(fail(name, format!(
            "one of the repeated tools/list requests omitted echo: rmcp first={}, tower first={}, rmcp second={}, tower second={}",
            rmcp_first.body, tower_first.body, rmcp_second.body, tower_second.body
        )));
    }
    if [&rmcp_first, &tower_first, &rmcp_second, &tower_second]
        .into_iter()
        .all(|response| response.session_id.is_none())
    {
        results.push(pass(
            name,
            "repeated requests remain stateless without session IDs",
        ));
    } else {
        results.push(fail(name, "a repeated final request created a session ID"));
    }

    let call_body = final_body(
        json!(103),
        "tools/call",
        json!({ "name": "echo", "arguments": { "message": "final hello" } }),
        false,
    );
    let (rmcp_call, tower_call) =
        match final_pair(client, "tools/call", Some("echo"), &call_body).await {
            Ok(pair) => pair,
            Err(error) => return vec![check_error(name, error.to_string())],
        };
    if rmcp_call.body["result"]["content"][0]["text"] == "final hello"
        && tower_call.body["result"]["content"][0]["text"] == "final hello"
    {
        results.push(pass(
            name,
            "tools/call returns the same complete echo result",
        ));
    } else {
        results.push(fail(
            name,
            format!(
                "echo result differs: rmcp={}, tower-mcp={}",
                rmcp_call.body, tower_call.body
            ),
        ));
    }
    results
}

async fn check_final_header_and_metadata_errors(client: &reqwest::Client) -> Vec<CheckResult> {
    let name = "final-validation";
    let mut results = Vec::new();
    let list_body = final_body(json!(104), "tools/list", json!({}), false);

    let (rmcp, tower) = match (
        post_mcp_with_headers(
            client,
            &RMCP_FINAL,
            &list_body,
            None,
            &[("mcp-method", "tools/list")],
        )
        .await,
        post_mcp_with_headers(
            client,
            &TOWER_MCP_FINAL,
            &list_body,
            None,
            &[("mcp-method", "tools/list")],
        )
        .await,
    ) {
        (Ok(rmcp), Ok(tower)) => (rmcp, tower),
        (Err(error), _) | (_, Err(error)) => return vec![check_error(name, error.to_string())],
    };
    if rmcp.status == 400
        && tower.status == 400
        && rmcp.body["error"]["code"] == -32020
        && tower.body["error"]["code"] == -32020
    {
        results.push(pass(
            name,
            "missing MCP-Protocol-Version is HTTP 400 / -32020 on both",
        ));
    } else {
        results.push(fail(
            name,
            format!(
                "missing MCP-Protocol-Version differs: rmcp={}/{}, tower-mcp={}/{}",
                rmcp.status, rmcp.body, tower.status, tower.body
            ),
        ));
    }

    let (rmcp, tower) = match (
        post_mcp_with_headers(
            client,
            &RMCP_FINAL,
            &list_body,
            None,
            &[("mcp-protocol-version", "2026-07-28")],
        )
        .await,
        post_mcp_with_headers(
            client,
            &TOWER_MCP_FINAL,
            &list_body,
            None,
            &[("mcp-protocol-version", "2026-07-28")],
        )
        .await,
    ) {
        (Ok(rmcp), Ok(tower)) => (rmcp, tower),
        (Err(error), _) | (_, Err(error)) => return vec![check_error(name, error.to_string())],
    };
    if rmcp.status == 400
        && tower.status == 400
        && rmcp.body["error"]["code"] == -32020
        && tower.body["error"]["code"] == -32020
    {
        results.push(pass(
            name,
            "missing Mcp-Method is HTTP 400 / -32020 on both",
        ));
    } else {
        results.push(fail(
            name,
            format!(
                "missing Mcp-Method differs: rmcp={}/{}, tower-mcp={}/{}",
                rmcp.status, rmcp.body, tower.status, tower.body
            ),
        ));
    }

    let call_body = final_body(
        json!(105),
        "tools/call",
        json!({ "name": "echo", "arguments": { "message": "hello" } }),
        false,
    );
    let (rmcp, tower) = match (
        post_mcp_with_headers(
            client,
            &RMCP_FINAL,
            &call_body,
            None,
            &[
                ("mcp-protocol-version", "2026-07-28"),
                ("mcp-method", "tools/call"),
            ],
        )
        .await,
        post_mcp_with_headers(
            client,
            &TOWER_MCP_FINAL,
            &call_body,
            None,
            &[
                ("mcp-protocol-version", "2026-07-28"),
                ("mcp-method", "tools/call"),
            ],
        )
        .await,
    ) {
        (Ok(rmcp), Ok(tower)) => (rmcp, tower),
        (Err(error), _) | (_, Err(error)) => return vec![check_error(name, error.to_string())],
    };
    if rmcp.status == 400
        && tower.status == 400
        && rmcp.body["error"]["code"] == -32020
        && tower.body["error"]["code"] == -32020
    {
        results.push(pass(name, "missing Mcp-Name is HTTP 400 / -32020 on both"));
    } else {
        results.push(fail(
            name,
            format!(
                "missing Mcp-Name differs: rmcp={}/{}, tower-mcp={}/{}",
                rmcp.status, rmcp.body, tower.status, tower.body
            ),
        ));
    }

    let missing_meta = json!({
        "jsonrpc": "2.0",
        "id": 106,
        "method": "tools/list",
        "params": {}
    })
    .to_string();
    let (rmcp, tower) = match final_pair(client, "tools/list", None, &missing_meta).await {
        Ok(pair) => pair,
        Err(error) => return vec![check_error(name, error.to_string())],
    };
    if rmcp.status == 400
        && tower.status == 400
        && rmcp.body["error"]["code"] == -32602
        && tower.body["error"]["code"] == -32602
    {
        results.push(pass(
            name,
            "missing required request metadata is HTTP 400 / -32602 on both",
        ));
    } else {
        results.push(fail(
            name,
            format!(
                "missing metadata differs: rmcp={}/{}, tower-mcp={}/{}",
                rmcp.status, rmcp.body, tower.status, tower.body
            ),
        ));
    }

    let mut unsupported_meta = final_meta(false);
    unsupported_meta["io.modelcontextprotocol/protocolVersion"] = json!("2099-01-01");
    let unsupported_body = json!({
        "jsonrpc": "2.0",
        "id": 107,
        "method": "tools/list",
        "params": { "_meta": unsupported_meta }
    })
    .to_string();
    let (rmcp, tower) = match (
        post_mcp_with_headers(
            client,
            &RMCP_FINAL,
            &unsupported_body,
            None,
            &[
                ("mcp-protocol-version", "2099-01-01"),
                ("mcp-method", "tools/list"),
            ],
        )
        .await,
        post_mcp_with_headers(
            client,
            &TOWER_MCP_FINAL,
            &unsupported_body,
            None,
            &[
                ("mcp-protocol-version", "2099-01-01"),
                ("mcp-method", "tools/list"),
            ],
        )
        .await,
    ) {
        (Ok(rmcp), Ok(tower)) => (rmcp, tower),
        (Err(error), _) | (_, Err(error)) => return vec![check_error(name, error.to_string())],
    };
    if rmcp.status >= 400 && tower.status >= 400 {
        results.push(pass(
            name,
            format!(
                "unsupported protocol version is rejected by both (rmcp HTTP {}, tower-mcp HTTP {})",
                rmcp.status, tower.status
            ),
        ));
    } else {
        results.push(fail(
            name,
            format!(
                "unsupported version accepted: rmcp={}/{}, tower-mcp={}/{}",
                rmcp.status, rmcp.body, tower.status, tower.body
            ),
        ));
    }
    results
}

async fn check_final_subscription(client: &reqwest::Client) -> Vec<CheckResult> {
    let name = "final-subscriptions";
    let body = final_body(
        json!("subscription-1"),
        "subscriptions/listen",
        json!({ "notifications": { "toolsListChanged": true } }),
        false,
    );
    let (rmcp, tower) = match (
        post_final_stream_first_message(client, &RMCP_FINAL, &body).await,
        post_final_stream_first_message(client, &TOWER_MCP_FINAL, &body).await,
    ) {
        (Ok(rmcp), Ok(tower)) => (rmcp, tower),
        (Err(error), _) | (_, Err(error)) => return vec![check_error(name, error.to_string())],
    };
    let mut results = Vec::new();
    if rmcp.status == 200
        && tower.status == 200
        && rmcp.content_type.contains("text/event-stream")
        && tower.content_type.contains("text/event-stream")
        && rmcp.session_id.is_none()
        && tower.session_id.is_none()
    {
        results.push(pass(name, "both open a stateless SSE response stream"));
    } else {
        results.push(fail(
            name,
            format!(
                "stream setup differs: rmcp={}/{}/{:?}, tower-mcp={}/{}/{:?}",
                rmcp.status,
                rmcp.content_type,
                rmcp.session_id,
                tower.status,
                tower.content_type,
                tower.session_id
            ),
        ));
    }
    let is_ack = |response: &ServerResponse| {
        response.body["method"] == "notifications/subscriptions/acknowledged"
            && response.body["params"]["notifications"]["toolsListChanged"] == true
            && response.body["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"]
                == "subscription-1"
    };
    if is_ack(&rmcp) && is_ack(&tower) {
        results.push(pass(
            name,
            "first stream event acknowledges the requested filter and ID",
        ));
    } else {
        results.push(fail(
            name,
            format!(
                "acknowledgment differs: rmcp={}, tower-mcp={}",
                rmcp.body, tower.body
            ),
        ));
    }
    results
}

async fn check_final_mrtr(client: &reqwest::Client) -> Vec<CheckResult> {
    let name = "final-mrtr";
    let first_body = final_body(
        json!(108),
        "tools/call",
        json!({ "name": "mrtr", "arguments": {} }),
        false,
    );
    let (rmcp_first, tower_first) =
        match final_pair(client, "tools/call", Some("mrtr"), &first_body).await {
            Ok(pair) => pair,
            Err(error) => return vec![check_error(name, error.to_string())],
        };
    let mut results = Vec::new();
    if rmcp_first.body["result"]["resultType"] == "input_required"
        && tower_first.body["result"]["resultType"] == "input_required"
        && rmcp_first.body["result"]["requestState"] == "compat-mrtr-state"
        && tower_first.body["result"]["requestState"] == "compat-mrtr-state"
    {
        results.push(pass(
            name,
            "both return the same input_required continuation",
        ));
    } else {
        results.push(fail(
            name,
            format!(
                "input-required result differs: rmcp={}, tower-mcp={}",
                rmcp_first.body, tower_first.body
            ),
        ));
        return results;
    }

    let retry_body = final_body(
        json!(109),
        "tools/call",
        json!({
            "name": "mrtr",
            "arguments": {},
            "requestState": "compat-mrtr-state"
        }),
        false,
    );
    let (rmcp_retry, tower_retry) =
        match final_pair(client, "tools/call", Some("mrtr"), &retry_body).await {
            Ok(pair) => pair,
            Err(error) => return vec![check_error(name, error.to_string())],
        };
    if rmcp_retry.body["result"]["content"][0]["text"] == "mrtr complete"
        && tower_retry.body["result"]["content"][0]["text"] == "mrtr complete"
    {
        results.push(pass(
            name,
            "byte-exact requestState retry completes on both",
        ));
    } else {
        results.push(fail(
            name,
            format!(
                "MRTR retry differs: rmcp={}, tower-mcp={}",
                rmcp_retry.body, tower_retry.body
            ),
        ));
    }
    results
}

async fn get_final_task(
    client: &reqwest::Client,
    server: &ServerConfig,
    id: u64,
    task_id: &str,
    tasks: bool,
) -> Result<ServerResponse> {
    let body = final_body(json!(id), "tasks/get", json!({ "taskId": task_id }), tasks);
    post_final(client, server, "tasks/get", Some(task_id), &body).await
}

async fn check_final_tasks(client: &reqwest::Client) -> Vec<CheckResult> {
    let name = "final-tasks";
    let unnegotiated = final_body(
        json!(110),
        "tasks/get",
        json!({ "taskId": "not-a-task" }),
        false,
    );
    let (rmcp_missing, tower_missing) =
        match final_pair(client, "tasks/get", Some("not-a-task"), &unnegotiated).await {
            Ok(pair) => pair,
            Err(error) => return vec![check_error(name, error.to_string())],
        };
    let mut results = Vec::new();
    if rmcp_missing.body["error"]["code"] == -32021 && tower_missing.body["error"]["code"] == -32021
    {
        results.push(pass(
            name,
            "tasks/get requires per-request Tasks negotiation on both",
        ));
    } else {
        results.push(fail(
            name,
            format!(
                "unnegotiated task error differs: rmcp={}, tower-mcp={}",
                rmcp_missing.body, tower_missing.body
            ),
        ));
    }

    let create_body = final_body(
        json!(111),
        "tools/call",
        json!({ "name": "slow_echo", "arguments": { "delay_ms": 1000 } }),
        true,
    );
    let (rmcp_create, tower_create) =
        match final_pair(client, "tools/call", Some("slow_echo"), &create_body).await {
            Ok(pair) => pair,
            Err(error) => return vec![check_error(name, error.to_string())],
        };
    let rmcp_task_id = rmcp_create.body["result"]["taskId"].as_str();
    let tower_task_id = tower_create.body["result"]["taskId"].as_str();
    let (Some(rmcp_task_id), Some(tower_task_id)) = (rmcp_task_id, tower_task_id) else {
        results.push(fail(
            name,
            format!(
                "task creation differs: rmcp={}, tower-mcp={}",
                rmcp_create.body, tower_create.body
            ),
        ));
        return results;
    };
    if rmcp_create.body["result"]["resultType"] == "task"
        && tower_create.body["result"]["resultType"] == "task"
    {
        results.push(pass(name, "negotiated tools/call creates a task on both"));
    } else {
        results.push(fail(name, "task creation omitted resultType=task"));
    }

    let rmcp_get = get_final_task(client, &RMCP_FINAL, 112, rmcp_task_id, true).await;
    let tower_get = get_final_task(client, &TOWER_MCP_FINAL, 112, tower_task_id, true).await;
    let (rmcp_get, tower_get) = match (rmcp_get, tower_get) {
        (Ok(rmcp), Ok(tower)) => (rmcp, tower),
        (Err(error), _) | (_, Err(error)) => return vec![check_error(name, error.to_string())],
    };
    if rmcp_get.body["result"]["taskId"] == rmcp_task_id
        && tower_get.body["result"]["taskId"] == tower_task_id
        && rmcp_get.body["result"]["status"].is_string()
        && tower_get.body["result"]["status"].is_string()
    {
        results.push(pass(name, "tasks/get returns each created task"));
    } else {
        results.push(fail(
            name,
            format!(
                "tasks/get differs: rmcp={}, tower-mcp={}",
                rmcp_get.body, tower_get.body
            ),
        ));
    }

    let cancel = |id: u64, task_id: &str| {
        final_body(
            json!(id),
            "tasks/cancel",
            json!({ "taskId": task_id }),
            true,
        )
    };
    let rmcp_cancel_body = cancel(113, rmcp_task_id);
    let tower_cancel_body = cancel(113, tower_task_id);
    let rmcp_cancel = post_final(
        client,
        &RMCP_FINAL,
        "tasks/cancel",
        Some(rmcp_task_id),
        &rmcp_cancel_body,
    )
    .await;
    let tower_cancel = post_final(
        client,
        &TOWER_MCP_FINAL,
        "tasks/cancel",
        Some(tower_task_id),
        &tower_cancel_body,
    )
    .await;
    let (rmcp_cancel, tower_cancel) = match (rmcp_cancel, tower_cancel) {
        (Ok(rmcp), Ok(tower)) => (rmcp, tower),
        (Err(error), _) | (_, Err(error)) => return vec![check_error(name, error.to_string())],
    };
    if rmcp_cancel.body.get("error").is_none() && tower_cancel.body.get("error").is_none() {
        results.push(pass(name, "tasks/cancel is acknowledged on both"));
    } else {
        results.push(fail(
            name,
            format!(
                "tasks/cancel differs: rmcp={}, tower-mcp={}",
                rmcp_cancel.body, tower_cancel.body
            ),
        ));
    }

    let mut rmcp_status = None;
    let mut tower_status = None;
    for attempt in 0..20 {
        let rmcp_poll =
            get_final_task(client, &RMCP_FINAL, 114 + attempt, rmcp_task_id, true).await;
        let tower_poll =
            get_final_task(client, &TOWER_MCP_FINAL, 114 + attempt, tower_task_id, true).await;
        if let (Ok(rmcp), Ok(tower)) = (rmcp_poll, tower_poll) {
            rmcp_status = rmcp.body["result"]["status"].as_str().map(str::to_owned);
            tower_status = tower.body["result"]["status"].as_str().map(str::to_owned);
            if rmcp_status.as_deref() == Some("cancelled")
                && tower_status.as_deref() == Some("cancelled")
            {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if rmcp_status.as_deref() == Some("cancelled") && tower_status.as_deref() == Some("cancelled") {
        results.push(pass(
            name,
            "cancelled lifecycle state is observable on both",
        ));
    } else {
        results.push(fail(
            name,
            format!("cancelled state differs: rmcp={rmcp_status:?}, tower-mcp={tower_status:?}"),
        ));
    }
    results
}

// ============================================================
// Main
// ============================================================

#[tokio::main]
async fn main() -> Result<()> {
    let rmcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let tower_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let tower_sse_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let rmcp_final_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let tower_final_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;

    RMCP_PORT.store(rmcp_listener.local_addr()?.port(), Ordering::Relaxed);
    TOWER_MCP_PORT.store(tower_listener.local_addr()?.port(), Ordering::Relaxed);
    TOWER_MCP_SSE_PORT.store(tower_sse_listener.local_addr()?.port(), Ordering::Relaxed);
    RMCP_FINAL_PORT.store(rmcp_final_listener.local_addr()?.port(), Ordering::Relaxed);
    TOWER_MCP_FINAL_PORT.store(tower_final_listener.local_addr()?.port(), Ordering::Relaxed);

    tokio::spawn(start_rmcp_server(rmcp_listener));
    tokio::spawn(start_tower_mcp_server(tower_listener));
    tokio::spawn(start_tower_mcp_sse_server(tower_sse_listener));
    tokio::spawn(start_rmcp_final_server(rmcp_final_listener));
    tokio::spawn(start_tower_mcp_final_server(tower_final_listener));

    println!(
        "Waiting for servers: {}, {}, {}, {}, and {}...",
        RMCP.display_name(),
        TOWER_MCP.display_name(),
        TOWER_MCP_SSE.display_name(),
        RMCP_FINAL.display_name(),
        TOWER_MCP_FINAL.display_name()
    );

    let rmcp_ready = wait_for_ready(&RMCP).await;
    let tower_ready = wait_for_ready(&TOWER_MCP).await;
    let tower_sse_ready = wait_for_ready(&TOWER_MCP_SSE).await;
    let rmcp_final_ready = wait_for_ready(&RMCP_FINAL).await;
    let tower_final_ready = wait_for_ready(&TOWER_MCP_FINAL).await;

    if !rmcp_ready {
        eprintln!("ERROR: {} did not become ready", RMCP.display_name());
        std::process::exit(1);
    }
    if !tower_ready {
        eprintln!("ERROR: {} did not become ready", TOWER_MCP.display_name());
        std::process::exit(1);
    }
    if !tower_sse_ready {
        eprintln!(
            "ERROR: {} did not become ready",
            TOWER_MCP_SSE.display_name()
        );
        std::process::exit(1);
    }
    if !rmcp_final_ready {
        eprintln!("ERROR: {} did not become ready", RMCP_FINAL.display_name());
        std::process::exit(1);
    }
    if !tower_final_ready {
        eprintln!(
            "ERROR: {} did not become ready",
            TOWER_MCP_FINAL.display_name()
        );
        std::process::exit(1);
    }

    println!("All servers ready. Running checks...\n");

    let client = reqwest::Client::new();
    let mut all_results: Vec<CheckResult> = Vec::new();

    println!("=== Stable protocol (2025-11-25) ===\n");

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

    println!("\n=== Final protocol (2026-07-28) ===\n");

    for (heading, results) in [
        ("server/discover", check_final_discover(&client).await),
        (
            "stateless tools",
            check_final_stateless_tools(&client).await,
        ),
        (
            "headers and metadata validation",
            check_final_header_and_metadata_errors(&client).await,
        ),
        (
            "subscriptions/listen",
            check_final_subscription(&client).await,
        ),
        ("MRTR", check_final_mrtr(&client).await),
        ("Tasks extension", check_final_tasks(&client).await),
    ] {
        println!("=== {heading} ===");
        for result in &results {
            print_result(result);
        }
        all_results.extend(results);
        println!();
    }

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
