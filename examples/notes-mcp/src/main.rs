//! Notes MCP Server
//!
//! A Redis-backed customer notes system demonstrating tower-mcp with
//! persistent storage (RedisJSON + RediSearch).

mod prompts;
mod resources;
mod seed;
mod state;
mod tools;

use std::sync::Arc;

use clap::{Parser, ValueEnum};
use tower_mcp::{McpRouter, StdioTransport};

use crate::state::AppState;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Transport {
    Stdio,
}

#[derive(Parser, Debug)]
#[command(name = "notes-mcp")]
#[command(about = "MCP server for Redis-backed customer notes", long_about = None)]
struct Args {
    /// Transport to use
    #[arg(short, long, default_value = "stdio")]
    transport: Transport,

    /// Redis connection URL
    #[arg(long, default_value = "redis://127.0.0.1:6379")]
    redis_url: String,

    /// Seed the database with sample data (creates indexes and loads demo customers/notes)
    #[arg(long)]
    seed: bool,

    /// Log level
    #[arg(short, long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    let args = Args::parse();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(format!("notes_mcp={}", args.log_level).parse()?)
                .add_directive(format!("tower_mcp={}", args.log_level).parse()?),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!(
        transport = ?args.transport,
        redis_url = %args.redis_url,
        "Starting notes-mcp server"
    );

    // Connect to Redis
    let state = Arc::new(AppState::new(&args.redis_url).await?);

    // Seed if requested
    if args.seed {
        tracing::info!("Seeding database with sample data...");
        seed::seed(state.conn()).await?;
        tracing::info!("Seeding complete");
    }

    // Build tools
    let search_customers = tools::search_customers::build(state.clone());
    let search_notes = tools::search_notes::build(state.clone());
    let list_customers = tools::list_customers::build(state.clone());
    let add_note = tools::add_note::build(state.clone());
    let get_customer = tools::get_customer::build(state.clone());
    let update_customer = tools::update_customer::build(state.clone());
    let update_note = tools::update_note::build(state.clone());
    let delete_note = tools::delete_note::build(state.clone());

    // Build resources
    let customer_template = resources::customer::build(state.clone());
    let recent_notes = resources::recent_notes::build(state.clone());

    // Build prompts
    let prep_meeting = prompts::prep_meeting::build();
    let account_review = prompts::account_review::build();

    // Assemble router
    let router = McpRouter::new()
        .server_info("notes-mcp", env!("CARGO_PKG_VERSION"))
        .instructions(
            "MCP server for managing customer notes backed by Redis.\n\n\
             Available tools:\n\
             - search_customers: Find customers by name, company, or role\n\
             - search_notes: Search note content with optional filters\n\
             - list_customers: List all customers with note counts\n\
             - get_customer: Get a customer's full profile and notes\n\
             - add_note: Create a new note for a customer\n\
             - update_customer: Update customer profile fields\n\
             - update_note: Update note content or metadata\n\
             - delete_note: Delete a note by ID\n\n\
             Resources:\n\
             - notes://customers/{id}: Customer profile with notes\n\
             - notes://recent: 10 most recent notes across all customers\n\n\
             Use the prep_meeting prompt for guided meeting preparation.\n\
             Use the account_review prompt for tier-level portfolio analysis.",
        )
        .tool(search_customers)
        .tool(search_notes)
        .tool(list_customers)
        .tool(add_note)
        .tool(get_customer)
        .tool(update_customer)
        .tool(update_note)
        .tool(delete_note)
        .resource_template(customer_template)
        .resource(recent_notes)
        .prompt(prep_meeting)
        .prompt(account_review);

    match args.transport {
        Transport::Stdio => {
            tracing::info!("Serving over stdio");
            StdioTransport::new(router).run().await?;
        }
    }

    Ok(())
}
