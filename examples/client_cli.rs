//! MCP Client CLI example
//!
//! Demonstrates using McpClient to connect to an MCP server and interact with it.
//!
//! This example spawns the stdio_server example as a subprocess and:
//! 1. Initializes the connection
//! 2. Lists available tools
//! 3. Calls each tool with sample inputs
//!
//! Run with: cargo run --example client_cli
//!
//! You can also connect to any MCP server by passing the command as arguments:
//!   cargo run --example client_cli -- /path/to/mcp-server --some-flag

use tower_mcp::client::{McpClient, StdioClientTransport};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=info,client_cli=debug")
        .init();

    // Get server command from args, or default to our stdio_server example
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, cmd_args): (&str, Vec<&str>) = if args.is_empty() {
        ("cargo", vec!["run", "--example", "stdio_server"])
    } else {
        (
            args[0].as_str(),
            args[1..].iter().map(|s| s.as_str()).collect(),
        )
    };

    println!("Connecting to MCP server: {} {:?}", cmd, cmd_args);
    println!();

    // Spawn and connect to the server
    let transport = StdioClientTransport::spawn(cmd, &cmd_args).await?;
    let mut client = McpClient::new(transport);

    // Initialize
    println!("Initializing connection...");
    let server_info = client.initialize("tower-mcp-client-cli", "0.1.0").await?;
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

        // Try echo tool
        if tools.tools.iter().any(|t| t.name == "echo") {
            println!("  Calling 'echo' with message: \"Hello from tower-mcp client!\"");
            let result = client
                .call_tool(
                    "echo",
                    serde_json::json!({"message": "Hello from tower-mcp client!"}),
                )
                .await?;
            print_result(&result);
        }

        // Try add tool
        if tools.tools.iter().any(|t| t.name == "add") {
            println!("  Calling 'add' with a=42, b=58");
            let result = client
                .call_tool("add", serde_json::json!({"a": 42, "b": 58}))
                .await?;
            print_result(&result);
        }

        // Try reverse tool
        if tools.tools.iter().any(|t| t.name == "reverse") {
            println!("  Calling 'reverse' with text: \"tower-mcp\"");
            let result = client
                .call_tool("reverse", serde_json::json!({"text": "tower-mcp"}))
                .await?;
            print_result(&result);
        }
    }

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
        }
    }
    println!();
}
