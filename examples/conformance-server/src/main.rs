mod prompts;
mod resources;
mod tools;

use tower_mcp::context::notification_channel;
use tower_mcp::{HttpTransport, McpRouter};

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=debug".parse()?),
        )
        .init();

    let (tx, _rx) = notification_channel(256);

    let mut router = McpRouter::new()
        .server_info("conformance-server", "0.1.0")
        .instructions("MCP conformance test server implementing all spec features")
        .with_notification_sender(tx);

    for tool in tools::build_tools() {
        router = router.tool(tool);
    }
    for resource in resources::build_resources() {
        router = router.resource(resource);
    }
    for template in resources::build_resource_templates() {
        router = router.resource_template(template);
    }
    for prompt in prompts::build_prompts() {
        router = router.prompt(prompt);
    }

    let transport = HttpTransport::new(router).with_sampling();

    let app = transport.into_router_at("/mcp");

    let addr = "127.0.0.1:3001";
    tracing::info!("Starting conformance server on http://{}/mcp", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
