//! tower-mcp-codegen: An MCP server that helps build tower-mcp servers.
//!
//! This server provides tools for AI agents to incrementally define and generate
//! tower-mcp server code. The workflow is:
//!
//! 1. `init_project` - Initialize a new project with name and transports
//! 2. `add_tool` - Add tools with input fields and handler types
//! 3. `get_project` - Inspect the current project state (or read project:// resources)
//! 4. `validate` - Verify the generated code compiles
//! 5. `generate` - Generate complete, compilable Rust code
//! 6. `reset` - Start over with a new project
//!
//! ## Resources
//!
//! - `project://Cargo.toml` - Generated Cargo.toml content
//! - `project://src/main.rs` - Generated main.rs content
//! - `project://state.json` - Current project state as JSON
//!
//! ## Example
//!
//! ```text
//! Agent: "Create an MCP server that echoes messages"
//!
//! 1. init_project(name="echo-server", transports=["stdio"])
//! 2. add_tool(
//!      name="echo",
//!      description="Echo a message back",
//!      input_fields=[{name: "message", type: "String", description: "Message to echo"}]
//!    )
//! 3. validate() -> Check it compiles
//! 4. generate() -> Complete Cargo.toml and main.rs
//! ```

mod codegen;
mod resources;
mod state;
mod tools;

use std::sync::Arc;

use clap::Parser;
use tower_mcp::{McpRouter, StdioTransport};

use resources::build_resources;
use tools::{CodegenState, build_tools};

#[derive(Parser)]
#[command(name = "codegen-mcp")]
#[command(about = "MCP server for generating tower-mcp server code")]
struct Args {
    /// Transport to use (stdio only for now)
    #[arg(long, default_value = "stdio")]
    transport: String,
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter("codegen_mcp=info,tower_mcp=info")
        .with_writer(std::io::stderr)
        .init();

    // Create shared state
    let state = Arc::new(CodegenState::new());

    // Build tools and resources
    let tools = build_tools(state.clone());
    let resources = build_resources(state);

    // Create router
    let mut router = McpRouter::new()
        .server_info("codegen-mcp", env!("CARGO_PKG_VERSION"))
        .instructions(
            "This server helps you build tower-mcp servers. Use init_project to start, \
             add_tool to define tools, and generate to get the code. Read project:// \
             resources to see generated files, or call get_project for the full state. \
             Use validate to check the code compiles, or reset to start over.",
        );

    for tool in tools {
        router = router.tool(tool);
    }

    for resource in resources {
        router = router.resource(resource);
    }

    // Run transport
    match args.transport.as_str() {
        "stdio" => {
            tracing::info!("Starting codegen-mcp server on stdio");
            let mut transport = StdioTransport::new(router);
            transport.run().await?;
        }
        _ => {
            eprintln!(
                "Unsupported transport: {}. Only 'stdio' is supported.",
                args.transport
            );
            std::process::exit(1);
        }
    }

    Ok(())
}
