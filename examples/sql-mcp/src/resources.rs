use std::collections::HashMap;
use std::sync::Arc;

use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
use tower_mcp::resource::{ResourceBuilder, ResourceTemplateBuilder};

use crate::db::{DatabaseManager, execute_query};

/// Static resource listing all configured database connections.
pub fn connections_resource(db: Arc<DatabaseManager>) -> tower_mcp::resource::Resource {
    let db_clone = db.clone();
    ResourceBuilder::new("db://connections")
        .name("Database Connections")
        .description("List all configured database connections and their settings")
        .mime_type("application/json")
        .handler(move || {
            let db = db_clone.clone();
            async move {
                let connections: Vec<serde_json::Value> = db
                    .names()
                    .iter()
                    .map(|name| {
                        let conn = db.get(name).unwrap();
                        serde_json::json!({
                            "name": name,
                            "read_only": conn.read_only,
                            "row_limit": conn.row_limit,
                            "query_timeout_seconds": conn.query_timeout_seconds,
                        })
                    })
                    .collect();

                let text = serde_json::to_string_pretty(&connections)?;
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: "db://connections".to_string(),
                        mime_type: Some("application/json".to_string()),
                        text: Some(text),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                })
            }
        })
        .build()
}

/// Resource template for listing tables in a specific database.
pub fn tables_resource_template(db: Arc<DatabaseManager>) -> tower_mcp::resource::ResourceTemplate {
    let db_clone = db.clone();
    ResourceTemplateBuilder::new("db://{connection}/tables")
        .name("Database Tables")
        .description("List tables in a database connection")
        .mime_type("application/json")
        .handler(move |uri: String, vars: HashMap<String, String>| {
            let db = db_clone.clone();
            async move {
                let connection = vars.get("connection").cloned().unwrap_or_default();
                let conn = db.get(&connection).ok_or_else(|| {
                    tower_mcp::Error::tool(format!("Unknown database '{connection}'"))
                })?;

                let sql = if DatabaseManager::is_sqlite(&conn.pool) {
                    "SELECT name, 'table' as type FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name".to_string()
                } else {
                    "SELECT table_name as name, table_schema as schema, table_type as type FROM information_schema.tables WHERE table_schema NOT IN ('information_schema', 'pg_catalog') ORDER BY table_schema, table_name".to_string()
                };

                let rows = execute_query(&conn.pool, &sql)
                    .await
                    .map_err(|e| tower_mcp::Error::tool(e.to_string()))?;
                let text = serde_json::to_string_pretty(&rows)?;

                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: Some("application/json".to_string()),
                        text: Some(text),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                })
            }
        })
}
