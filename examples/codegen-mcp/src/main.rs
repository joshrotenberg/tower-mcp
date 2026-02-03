//! tower-mcp-codegen: An MCP server that helps build tower-mcp servers.
//!
//! This server provides two workflows for AI agents:
//!
//! ## Workflow 1: Component Builder (Recommended for agents)
//!
//! Build individual tools/resources/prompts incrementally with explicit variant discovery:
//!
//! ```text
//! 1. list_handler_types()  -> Learn available handler types
//! 2. list_input_types()    -> Learn available input types
//! 3. tool_new("search")    -> Start building, get an ID
//! 4. tool_set_description(id, "Search the database")
//! 5. tool_add_input(id, "query", "String", "Search term", true)
//! 6. tool_set_handler(id, "with_state")
//! 7. tool_add_layer(id, "timeout", {"secs": 30})
//! 8. tool_build(id)        -> Get Rust code for this tool
//! ```
//!
//! Benefits:
//! - Explicit variant discovery (no guessing what options exist)
//! - Incremental construction (one decision at a time)
//! - Immediate feedback at each step
//! - Can generate code for individual components
//!
//! ## Workflow 2: Project Builder (Original)
//!
//! Build a complete project with all tools at once:
//!
//! ```text
//! 1. init_project(name="echo-server", transports=["stdio"])
//! 2. add_tool(name="echo", description="...", input_fields=[...])
//! 3. validate() -> Check it compiles
//! 4. generate() -> Complete Cargo.toml and main.rs
//! ```
//!
//! ## Combining Workflows
//!
//! Use `tool_add_to_project(id)` to add a component-built tool to the project,
//! then use `generate()` for full project code.
//!
//! ## Resources
//!
//! - `project://Cargo.toml` - Generated Cargo.toml content
//! - `project://src/main.rs` - Generated main.rs content
//! - `project://state.json` - Current project state as JSON

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
            "This server helps you build tower-mcp servers.\n\n\
             **Recommended workflow (component builder):**\n\
             1. Call list_handler_types, list_input_types, list_layer_types to learn available options\n\
             2. Call tool_new(name) to start building a tool\n\
             3. Use tool_set_description, tool_add_input, tool_set_handler, tool_add_layer to configure\n\
             4. Call tool_build(id) to generate code for just that tool\n\n\
             **Alternative workflow (full project):**\n\
             1. init_project(name, transports) to start\n\
             2. add_tool(...) to add tools\n\
             3. generate() for complete project code\n\n\
             Use tool_list to see tools being built, tool_add_to_project to add to project.",
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
