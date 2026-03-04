//! HTTP MCP Client CLI example
//!
//! Demonstrates using McpClient with HttpClientTransport to connect to
//! a remote MCP server over Streamable HTTP.
//!
//! Run the HTTP server first:
//!   cargo run --example http_server --features http
//!
//! Then run this client:
//!   cargo run --example http_client_cli --features http-client
//!
//! You can also connect to any HTTP MCP server by passing the URL:
//!   cargo run --example http_client_cli --features http-client -- http://example.com:3000

use tower_mcp::client::{HttpClientTransport, McpClient};

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=info,http_client_cli=debug")
        .init();

    // Get server URL from args, or default to localhost
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string());

    println!("Connecting to MCP server at: {}", url);
    println!();

    // Connect to the server
    let transport = HttpClientTransport::new(&url);
    let client = McpClient::connect(transport).await?;

    // Initialize
    println!("Initializing connection...");
    let server_info = client.initialize("tower-mcp-http-client", "0.1.0").await?;
    println!(
        "Connected to: {} v{}",
        server_info.server_info.name, server_info.server_info.version
    );
    if let Some(instructions) = &server_info.instructions {
        println!("Instructions: {}", instructions);
    }
    println!();

    // Ping
    println!("Pinging server...");
    client.ping().await?;
    println!("Pong!");
    println!();

    // List tools
    println!("Listing tools...");
    let tools = client.list_tools().await?;
    println!("Available tools ({}):", tools.tools.len());
    for tool in &tools.tools {
        println!(
            "  - {} : {}",
            tool.name,
            tool.description.as_deref().unwrap_or("(no description)")
        );
    }
    println!();

    // List resources
    println!("Listing resources...");
    let resources = client.list_resources().await?;
    if resources.resources.is_empty() {
        println!("  (no resources)");
    } else {
        println!("Available resources ({}):", resources.resources.len());
        for resource in &resources.resources {
            println!("  - {} : {}", resource.uri, resource.name);
        }
    }
    println!();

    // List prompts
    println!("Listing prompts...");
    let prompts = client.list_prompts().await?;
    if prompts.prompts.is_empty() {
        println!("  (no prompts)");
    } else {
        println!("Available prompts ({}):", prompts.prompts.len());
        for prompt in &prompts.prompts {
            println!(
                "  - {} : {}",
                prompt.name,
                prompt.description.as_deref().unwrap_or("(no description)")
            );
        }
    }
    println!();

    // Call tools if they exist
    if !tools.tools.is_empty() {
        println!("Calling tools...");
        println!();

        if tools.tools.iter().any(|t| t.name == "echo") {
            println!("  Calling 'echo' with message: \"Hello from tower-mcp HTTP client!\"");
            let result = client
                .call_tool(
                    "echo",
                    serde_json::json!({"message": "Hello from tower-mcp HTTP client!"}),
                )
                .await?;
            print_result(&result);
        }

        if tools.tools.iter().any(|t| t.name == "add") {
            println!("  Calling 'add' with a=42, b=58");
            let result = client
                .call_tool("add", serde_json::json!({"a": 42, "b": 58}))
                .await?;
            print_result(&result);
        }
    }

    // Graceful shutdown
    println!("Shutting down...");
    client.shutdown().await?;
    println!("Done!");
    Ok(())
}

fn print_result(result: &tower_mcp::CallToolResult) {
    for content in &result.content {
        match content {
            tower_mcp::Content::Text { text, .. } => {
                println!("    Result: {}", text);
            }
            tower_mcp::Content::Image { .. } => {
                println!("    Result: (image)");
            }
            tower_mcp::Content::Audio { .. } => {
                println!("    Result: (audio)");
            }
            tower_mcp::Content::Resource { .. } => {
                println!("    Result: (embedded resource)");
            }
            tower_mcp::Content::ResourceLink { .. } => {
                println!("    Result: (resource link)");
            }
            _ => {
                println!("    Result: (unknown content type)");
            }
        }
    }
    println!();
}
