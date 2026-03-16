use std::collections::HashMap;
use std::sync::Arc;

use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
use tower_mcp::resource::{ResourceBuilder, ResourceTemplateBuilder};

use crate::db::{DatabaseManager, Dialect, execute_query};

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
                            "dialect": format!("{:?}", conn.dialect).to_lowercase(),
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

                let sql = match conn.dialect {
                    Dialect::Sqlite => "SELECT name, 'table' as type FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
                    Dialect::Postgres => "SELECT table_name as name, table_schema as schema, table_type as type FROM information_schema.tables WHERE table_schema NOT IN ('information_schema', 'pg_catalog') ORDER BY table_schema, table_name",
                    Dialect::Mysql => "SELECT table_name as name, table_schema as schema, table_type as type FROM information_schema.tables WHERE table_schema = DATABASE() ORDER BY table_name",
                };

                let rows = execute_query(&conn.pool, sql)
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

/// Resource template for listing schemas in a specific database.
pub fn schemas_resource_template(
    db: Arc<DatabaseManager>,
) -> tower_mcp::resource::ResourceTemplate {
    let db_clone = db.clone();
    ResourceTemplateBuilder::new("db://{connection}/schemas")
        .name("Database Schemas")
        .description("List schemas in a database connection")
        .mime_type("application/json")
        .handler(move |uri: String, vars: HashMap<String, String>| {
            let db = db_clone.clone();
            async move {
                let connection = vars.get("connection").cloned().unwrap_or_default();
                let conn = db.get(&connection).ok_or_else(|| {
                    tower_mcp::Error::tool(format!("Unknown database '{connection}'"))
                })?;

                let rows = match conn.dialect {
                    Dialect::Sqlite => {
                        vec![serde_json::json!({"name": "main"})]
                    }
                    Dialect::Postgres => {
                        execute_query(
                            &conn.pool,
                            "SELECT schema_name as name FROM information_schema.schemata \
                             WHERE schema_name NOT IN ('information_schema', 'pg_catalog', 'pg_toast') \
                             ORDER BY schema_name",
                        )
                        .await
                        .map_err(|e| tower_mcp::Error::tool(e.to_string()))?
                    }
                    Dialect::Mysql => {
                        execute_query(
                            &conn.pool,
                            "SELECT schema_name as name FROM information_schema.schemata \
                             WHERE schema_name NOT IN ('information_schema', 'performance_schema', 'mysql', 'sys') \
                             ORDER BY schema_name",
                        )
                        .await
                        .map_err(|e| tower_mcp::Error::tool(e.to_string()))?
                    }
                };

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

/// Resource template for a specific table's full definition.
pub fn table_detail_resource_template(
    db: Arc<DatabaseManager>,
) -> tower_mcp::resource::ResourceTemplate {
    let db_clone = db.clone();
    ResourceTemplateBuilder::new("db://{connection}/table/{name}")
        .name("Table Definition")
        .description("Full table definition including columns, types, indexes, and constraints")
        .mime_type("application/json")
        .handler(move |uri: String, vars: HashMap<String, String>| {
            let db = db_clone.clone();
            async move {
                let connection = vars.get("connection").cloned().unwrap_or_default();
                let table_name = vars.get("name").cloned().unwrap_or_default();
                let conn = db.get(&connection).ok_or_else(|| {
                    tower_mcp::Error::tool(format!("Unknown database '{connection}'"))
                })?;

                // Get columns
                let col_sql = match conn.dialect {
                    Dialect::Sqlite => format!("PRAGMA table_info('{table_name}')"),
                    Dialect::Postgres => format!(
                        "SELECT column_name, data_type, is_nullable, column_default \
                         FROM information_schema.columns \
                         WHERE table_name = '{table_name}' \
                         AND table_schema = 'public' \
                         ORDER BY ordinal_position"
                    ),
                    Dialect::Mysql => format!(
                        "SELECT column_name, data_type, is_nullable, column_default \
                         FROM information_schema.columns \
                         WHERE table_name = '{table_name}' \
                         AND table_schema = DATABASE() \
                         ORDER BY ordinal_position"
                    ),
                };

                let columns = execute_query(&conn.pool, &col_sql)
                    .await
                    .map_err(|e| tower_mcp::Error::tool(e.to_string()))?;

                // Get indexes (best-effort)
                let idx_sql = match conn.dialect {
                    Dialect::Sqlite => format!("PRAGMA index_list('{table_name}')"),
                    Dialect::Postgres => format!(
                        "SELECT indexname, indexdef FROM pg_indexes \
                         WHERE tablename = '{table_name}' AND schemaname = 'public'"
                    ),
                    Dialect::Mysql => format!(
                        "SELECT index_name, GROUP_CONCAT(column_name ORDER BY seq_in_index) as columns, non_unique \
                         FROM information_schema.statistics \
                         WHERE table_name = '{table_name}' AND table_schema = DATABASE() \
                         GROUP BY index_name, non_unique"
                    ),
                };

                let indexes = execute_query(&conn.pool, &idx_sql).await.unwrap_or_default();

                let result = serde_json::json!({
                    "table": table_name,
                    "columns": columns,
                    "indexes": indexes,
                });

                let text = serde_json::to_string_pretty(&result)?;
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
