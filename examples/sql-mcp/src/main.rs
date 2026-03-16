//! sql-mcp -- SQL database MCP server with tower middleware for safety
//!
//! A multi-database MCP server supporting Postgres, MySQL, and SQLite via sqlx.
//! Safety features (read-only mode, row limits, query timeouts) are implemented
//! as tower middleware layers, not application logic.
//!
//! Run with a config file:
//!   cargo run -p sql-mcp -- --config examples/sql-mcp/config.toml
//!
//! Or with an inline database:
//!   cargo run -p sql-mcp -- --sqlite /tmp/test.db
//!   cargo run -p sql-mcp -- --postgres postgres://localhost/mydb
//!   cargo run -p sql-mcp -- --mysql mysql://localhost/mydb

mod config;
mod db;
mod prompts;
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

    /// Quick-start: connect to a MySQL database URL
    #[arg(long)]
    mysql: Option<String>,
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
        let mut databases = Vec::new();

        if let Some(path) = &cli.sqlite {
            databases.push(config::DatabaseConfig {
                name: "default".to_string(),
                url: format!("sqlite://{path}"),
                read_only: None,
                row_limit: None,
                query_timeout_seconds: None,
                pool_size: 1,
            });
        }

        if let Some(url) = &cli.postgres {
            databases.push(config::DatabaseConfig {
                name: "default".to_string(),
                url: url.clone(),
                read_only: None,
                row_limit: None,
                query_timeout_seconds: None,
                pool_size: 5,
            });
        }

        if let Some(url) = &cli.mysql {
            databases.push(config::DatabaseConfig {
                name: "default".to_string(),
                url: url.clone(),
                read_only: None,
                row_limit: None,
                query_timeout_seconds: None,
                pool_size: 5,
            });
        }

        if databases.is_empty() {
            anyhow::bail!(
                "Provide --config, --sqlite, --postgres, or --mysql. \
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
             Use `list_connections` to see all connections and their settings. \
             Use `list_tables` and `describe_table` for schema discovery. \
             Use `query` to execute SQL. Use `explain` to check query plans. \
             Read `db://connections` or `db://{{name}}/tables` resources for context.",
            db.names().join(", ")
        ))
        // Query tools
        .tool(tools::query_tool(Arc::clone(&db), default_timeout))
        .tool(tools::explain_tool(Arc::clone(&db), default_timeout))
        // Schema discovery tools
        .tool(tools::list_schemas_tool(Arc::clone(&db)))
        .tool(tools::list_tables_tool(Arc::clone(&db)))
        .tool(tools::describe_table_tool(Arc::clone(&db)))
        .tool(tools::list_indexes_tool(Arc::clone(&db)))
        // Connection management tools
        .tool(tools::list_connections_tool(Arc::clone(&db)))
        .tool(tools::test_connection_tool(Arc::clone(&db)))
        // Resources
        .resource(resources::connections_resource(Arc::clone(&db)))
        .resource_template(resources::tables_resource_template(Arc::clone(&db)))
        .resource_template(resources::schemas_resource_template(Arc::clone(&db)))
        .resource_template(resources::table_detail_resource_template(Arc::clone(&db)))
        // Prompts
        .prompt(prompts::sql_assistant_prompt(Arc::clone(&db)));

    eprintln!("sql-mcp ready. Databases: {}", db.names().join(", "));
    StdioTransport::new(router).run().await?;

    Ok(())
}
