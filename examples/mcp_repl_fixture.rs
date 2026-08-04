//! Deterministic process fixture for `mcp-repl`'s black-box tests.
//!
//! This is intentionally an example in the unpublished examples package,
//! rather than another binary in the published `mcp-repl` crate. With no
//! arguments it serves stdio. `--http` binds an ephemeral localhost port and
//! writes the resulting URL to `MCP_REPL_FIXTURE_READY_FILE`.

use std::path::PathBuf;
use std::time::Duration;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, HttpTransport, McpRouter, PromptBuilder, ProtocolSupport, ResourceBuilder,
    StdioTransport, TaskSupportMode, ToolBuilder,
    extract::{Context, RawArgs},
    protocol::{LogLevel, LoggingMessageParams},
};

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

fn fixture_router() -> McpRouter {
    McpRouter::new()
        .server_info("mcp-repl-fixture", "1.0.0")
        .with_tasks()
        .tool(
            ToolBuilder::new("add")
                .description("Add two integers")
                .handler(|input: AddInput| async move {
                    Ok(CallToolResult::text((input.a + input.b).to_string()))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("slow_add")
                .description("Add two integers in a final-protocol task")
                .task_support(TaskSupportMode::Optional)
                .handler(|input: AddInput| async move {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    Ok(CallToolResult::text((input.a + input.b).to_string()))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("announce")
                .description("Emit a deterministic server log notification")
                .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
                    ctx.send_log(LoggingMessageParams::new(
                        LogLevel::Info,
                        serde_json::json!("fixture announcement"),
                    ));
                    // Let the notification frame reach the client before the
                    // one-shot process receives its terminal tool response.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok(CallToolResult::text("announced"))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("fail")
                .description("Return a deterministic MCP tool error")
                .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                    Ok(CallToolResult::error("fixture tool failure"))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("process_info")
                .description("Report deterministic process environment for import tests")
                .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                    let cwd = std::env::current_dir()
                        .expect("fixture current directory")
                        .to_string_lossy()
                        .into_owned();
                    let imported = std::env::var("MCP_REPL_IMPORTED_VALUE").ok();
                    Ok(CallToolResult::text(
                        serde_json::json!({ "cwd": cwd, "imported": imported }).to_string(),
                    ))
                })
                .build(),
        )
        .resource(
            ResourceBuilder::new("fixture://guide")
                .name("Fixture Guide")
                .description("A deterministic resource for process tests")
                .mime_type("text/plain")
                .text("fixture resource body"),
        )
        .prompt(
            PromptBuilder::new("greet")
                .description("Generate a deterministic greeting request")
                .required_arg("name", "The person to greet")
                .handler(|args| async move {
                    let name = args.get("name").map(String::as_str).unwrap_or("World");
                    Ok(tower_mcp::GetPromptResult::user_message(format!(
                        "Please greet {name} warmly."
                    )))
                })
                .build(),
        )
}

fn protocol_support() -> ProtocolSupport {
    ProtocolSupport::try_new(["2025-11-25", "2026-07-28"]).expect("fixture protocols are compiled")
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

fn write_marker(name: &str, contents: impl AsRef<[u8]>) {
    if let Some(path) = env_path(name) {
        // Publish markers atomically so the test never observes the empty
        // file between create/truncate and the content write on a busy host.
        let pending = path.with_extension("pending");
        std::fs::write(&pending, contents).expect("write pending fixture marker");
        std::fs::rename(pending, path).expect("publish fixture marker");
    }
}

async fn observe_subscription(request: Request, next: Next) -> Response {
    if request
        .headers()
        .get("mcp-method")
        .and_then(|value| value.to_str().ok())
        == Some("subscriptions/listen")
    {
        write_marker("MCP_REPL_FIXTURE_SUBSCRIPTION_FILE", b"seen");
    }
    next.run(request).await
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    eprintln!("mcp-repl fixture ready");
    if std::env::args().any(|arg| arg == "--http") {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/", listener.local_addr()?);
        write_marker("MCP_REPL_FIXTURE_READY_FILE", &url);
        let app = HttpTransport::new(fixture_router())
            .protocol_support(protocol_support())
            .disable_origin_validation()
            .into_router()
            .layer(axum::middleware::from_fn(observe_subscription));
        axum::serve(listener, app).await?;
    } else {
        StdioTransport::new(fixture_router())
            .protocol_support(protocol_support())
            .run()
            .await?;
        write_marker("MCP_REPL_FIXTURE_EXIT_FILE", b"clean");
    }
    Ok(())
}
