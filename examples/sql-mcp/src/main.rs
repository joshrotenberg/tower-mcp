//! sql-mcp -- SQL database MCP server with tower middleware for safety
//!
//! A multi-database MCP server supporting Postgres and SQLite via sqlx.
//! Safety features (read-only mode, row limits, query timeouts) are implemented
//! as tower middleware layers, not application logic.
//!
//! Run with a config file:
//!   cargo run -p sql-mcp -- --config examples/sql-mcp/config.toml
//!
//! Or with an inline SQLite database:
//!   cargo run -p sql-mcp -- --sqlite /tmp/test.db

mod config;
mod db;
mod resources;
mod safety;
mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tower_mcp::{McpRouter, StdioTransport};

#[derive(Parser)]
#[command(name = "sql-mcp", about = "SQL database MCP server")]
struct Cli {
    /// Path to TOML config file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Quick-start: connect to a SQLite database file
    #[arg(long)]
    sqlite: Option<String>,

    /// Quick-start: connect to a Postgres database URL
    #[arg(long)]
    postgres: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sql_mcp=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let config = if let Some(config_path) = &cli.config {
        config::Config::load(config_path)?
    } else {
        // Build config from CLI flags
        let mut databases = Vec::new();

        if let Some(sqlite_path) = &cli.sqlite {
            databases.push(config::DatabaseConfig {
                name: "default".to_string(),
                url: format!("sqlite://{sqlite_path}"),
                read_only: None,
                row_limit: None,
                query_timeout_seconds: None,
                pool_size: 1,
            });
        }

        if let Some(pg_url) = &cli.postgres {
            databases.push(config::DatabaseConfig {
                name: "default".to_string(),
                url: pg_url.clone(),
                read_only: None,
                row_limit: None,
                query_timeout_seconds: None,
                pool_size: 5,
            });
        }

        if databases.is_empty() {
            anyhow::bail!(
                "Provide --config, --sqlite, or --postgres. \
                 Example: sql-mcp --sqlite /tmp/test.db"
            );
        }

        config::Config {
            server: config::ServerConfig::default(),
            safety: config::SafetyConfig::default(),
            databases,
        }
    };

    let db = db::DatabaseManager::from_config(&config).await?;
    let default_timeout = config.safety.query_timeout_seconds;

    let router = McpRouter::new()
        .server_info(&config.server.name, &config.server.version)
        .instructions(format!(
            "SQL database server. Available databases: {}. \
             Use the `query` tool to execute SQL, `list_tables` to see available tables, \
             and `describe_table` to see column details. \
             Read the `db://connections` resource for connection settings.",
            db.names().join(", ")
        ))
        // Tools
        .tool(tools::query_tool(Arc::clone(&db), default_timeout))
        .tool(tools::list_tables_tool(Arc::clone(&db)))
        .tool(tools::describe_table_tool(Arc::clone(&db)))
        // Resources
        .resource(resources::connections_resource(Arc::clone(&db)))
        .resource_template(resources::tables_resource_template(Arc::clone(&db)));

    eprintln!("sql-mcp ready. Databases: {}", db.names().join(", "));
    StdioTransport::new(router).run().await?;

    Ok(())
}
