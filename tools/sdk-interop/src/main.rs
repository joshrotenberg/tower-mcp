use std::env;

use anyhow::{Context, Result, bail, ensure};
use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, GetPromptResult, HttpTransport, McpRouter, PromptBuilder, ProtocolSupport,
    ResourceBuilder, ToolBuilder,
    client::{HttpClientTransport, McpClient},
};

const FINAL_PROTOCOL: &str = "2026-07-28";
const STABLE_PROTOCOL: &str = "2025-11-25";
const TOOL_NAME: &str = "interop_add";
const RESOURCE_URI: &str = "interop://fixture";
const PROMPT_NAME: &str = "interop_greet";

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

fn build_router() -> McpRouter {
    let add = ToolBuilder::new(TOOL_NAME)
        .description("Add two integers for SDK interoperability testing")
        .read_only()
        .idempotent()
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text((input.a + input.b).to_string()))
        })
        .build();

    let fixture = ResourceBuilder::new(RESOURCE_URI)
        .name("SDK interoperability fixture")
        .description("Static content shared by every SDK interoperability server")
        .mime_type("text/plain")
        .text("sdk-interop resource");

    let greet = PromptBuilder::new(PROMPT_NAME)
        .description("Render a greeting for SDK interoperability testing")
        .required_arg("name", "Name to greet")
        .handler(|args| async move {
            let name = args.get("name").map(String::as_str).unwrap_or("World");
            Ok(GetPromptResult::user_message(format!("Hello, {name}!")))
        })
        .build();

    McpRouter::new()
        .server_info("tower-mcp-sdk-interop", env!("CARGO_PKG_VERSION"))
        .tool(add)
        .resource(fixture)
        .prompt(greet)
}

async fn serve(port: u16) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("failed to bind tower-mcp server on port {port}"))?;
    let address = listener.local_addr()?;
    let transport = HttpTransport::new(build_router())
        .protocol_support(ProtocolSupport::compiled())
        .disable_origin_validation();

    println!("READY http://{address}");
    axum::serve(listener, transport.into_router()).await?;
    Ok(())
}

async fn run_client(url: &str, protocol: &str) -> Result<()> {
    let support = ProtocolSupport::try_new([protocol])?;
    let client = McpClient::builder()
        .protocol_support(support)
        .connect_simple(HttpClientTransport::new(url))
        .await?;

    match protocol {
        FINAL_PROTOCOL => {
            let discovery = client
                .discover("tower-mcp-sdk-interop-client", env!("CARGO_PKG_VERSION"))
                .await?;
            ensure!(
                discovery
                    .supported_versions
                    .iter()
                    .any(|version| version == FINAL_PROTOCOL),
                "server/discover did not advertise {FINAL_PROTOCOL}: {:?}",
                discovery.supported_versions
            );
        }
        STABLE_PROTOCOL => {
            let initialized = client
                .initialize("tower-mcp-sdk-interop-client", env!("CARGO_PKG_VERSION"))
                .await?;
            ensure!(
                initialized.protocol_version == STABLE_PROTOCOL,
                "initialize negotiated {}, expected {STABLE_PROTOCOL}",
                initialized.protocol_version
            );
        }
        other => bail!("unsupported interoperability protocol: {other}"),
    }

    let tools = client.list_tools().await?;
    ensure!(
        tools.tools.iter().any(|tool| tool.name == TOOL_NAME),
        "tools/list omitted {TOOL_NAME}"
    );

    let called = client
        .call_tool(TOOL_NAME, serde_json::json!({ "a": 19, "b": 23 }))
        .await?;
    ensure!(
        called.first_text() == Some("42"),
        "tools/call returned unexpected content: {}",
        serde_json::to_string(&called)?
    );

    let resources = client.list_resources().await?;
    ensure!(
        resources
            .resources
            .iter()
            .any(|resource| resource.uri.as_str() == RESOURCE_URI),
        "resources/list omitted {RESOURCE_URI}"
    );
    let resource = client.read_resource(RESOURCE_URI).await?;
    ensure!(
        serde_json::to_string(&resource)?.contains("sdk-interop resource"),
        "resources/read returned unexpected content: {}",
        serde_json::to_string(&resource)?
    );

    let prompts = client.list_prompts().await?;
    ensure!(
        prompts
            .prompts
            .iter()
            .any(|prompt| prompt.name == PROMPT_NAME),
        "prompts/list omitted {PROMPT_NAME}"
    );
    let prompt = client
        .get_prompt(
            PROMPT_NAME,
            Some(std::collections::HashMap::from([(
                "name".to_string(),
                "Tower".to_string(),
            )])),
        )
        .await?;
    ensure!(
        serde_json::to_string(&prompt)?.contains("Hello, Tower!"),
        "prompts/get returned unexpected content: {}",
        serde_json::to_string(&prompt)?
    );

    client.shutdown().await?;
    println!("PASS tower-mcp client -> {url} ({protocol})");
    Ok(())
}

fn usage() -> ! {
    eprintln!("usage: sdk-interop server <port> | client <url> <2025-11-25|2026-07-28>");
    std::process::exit(2);
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("server") => {
            let port = args
                .next()
                .unwrap_or_else(|| usage())
                .parse::<u16>()
                .context("port must be an unsigned 16-bit integer")?;
            ensure!(
                args.next().is_none(),
                "server accepts only one port argument"
            );
            serve(port).await
        }
        Some("client") => {
            let url = args.next().unwrap_or_else(|| usage());
            let protocol = args.next().unwrap_or_else(|| usage());
            ensure!(
                args.next().is_none(),
                "client accepts only a URL and protocol"
            );
            run_client(&url, &protocol).await
        }
        _ => usage(),
    }
}
