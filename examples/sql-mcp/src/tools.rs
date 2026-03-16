use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tower::timeout::TimeoutLayer;
use tower_mcp::extract::{Json, State};
use tower_mcp::tool::Tool;
use tower_mcp::{CallToolResult, ToolBuilder};

use crate::db::{DatabaseManager, Dialect, execute_query};
use crate::safety;

// --- Input types ---

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryInput {
    /// Database connection name
    pub database: String,
    /// SQL query to execute
    pub sql: String,
    /// Optional bind parameters (positional)
    #[serde(default)]
    pub params: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DatabaseInput {
    /// Database connection name
    pub database: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TableInput {
    /// Database connection name
    pub database: String,
    /// Table name
    pub table: String,
    /// Optional schema name (default: public for Postgres, ignored for SQLite)
    pub schema: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExplainInput {
    /// Database connection name
    pub database: String,
    /// SQL query to explain
    pub sql: String,
    /// Run EXPLAIN ANALYZE (requires write access, default: false)
    #[serde(default)]
    pub analyze: bool,
}

// --- Output types ---

#[derive(Debug, Serialize, JsonSchema)]
pub struct QueryResult {
    pub rows: Vec<serde_json::Value>,
    pub row_count: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TableInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(rename = "type")]
    pub table_type: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SchemaInfo {
    pub name: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct IndexInfo {
    pub name: String,
    pub table: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<String>,
    pub unique: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TableDescription {
    pub table: String,
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexInfo>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ConnectionInfo {
    pub name: String,
    pub dialect: String,
    pub read_only: bool,
    pub row_limit: u32,
    pub query_timeout_seconds: u64,
    pub connected: bool,
}

// --- Helper ---

fn unknown_db_error(db: &DatabaseManager, name: &str) -> CallToolResult {
    CallToolResult::error(format!(
        "Unknown database '{}'. Available: {}",
        name,
        db.names().join(", ")
    ))
}

// --- Tools ---

pub fn query_tool(db: Arc<DatabaseManager>, default_timeout: u64) -> Tool {
    let output_schema =
        serde_json::to_value(schemars::schema_for!(QueryResult)).unwrap_or_default();
    ToolBuilder::new("query")
        .description(
            "Execute a SQL query against a database. Read-only databases only allow \
             SELECT and EXPLAIN. Row limits are enforced automatically.",
        )
        .output_schema(output_schema)
        .extractor_handler(
            db,
            |State(db): State<Arc<DatabaseManager>>, Json(input): Json<QueryInput>| async move {
                let Some(conn) = db.get(&input.database) else {
                    return Ok(unknown_db_error(&db, &input.database));
                };

                if conn.read_only
                    && let Err(msg) = safety::validate_read_only(&input.sql)
                {
                    return Ok(CallToolResult::error(msg));
                }

                let sql = safety::enforce_row_limit(&input.sql, conn.row_limit);

                match execute_query(&conn.pool, &sql).await {
                    Ok(rows) => {
                        let row_count = rows.len();
                        CallToolResult::from_serialize(&QueryResult { rows, row_count })
                    }
                    Err(e) => Ok(CallToolResult::error(format!("Query failed: {e}"))),
                }
            },
        )
        .layer(TimeoutLayer::new(Duration::from_secs(default_timeout)))
        .build()
}

pub fn explain_tool(db: Arc<DatabaseManager>, default_timeout: u64) -> Tool {
    ToolBuilder::new("explain")
        .description(
            "Show the execution plan for a SQL query. Use analyze=true for actual \
             runtime statistics (requires write access).",
        )
        .extractor_handler(
            db,
            |State(db): State<Arc<DatabaseManager>>, Json(input): Json<ExplainInput>| async move {
                let Some(conn) = db.get(&input.database) else {
                    return Ok(unknown_db_error(&db, &input.database));
                };

                if input.analyze && conn.read_only {
                    return Ok(CallToolResult::error(
                        "EXPLAIN ANALYZE requires write access. \
                         This database is configured as read-only.",
                    ));
                }

                let sql = match conn.dialect {
                    Dialect::Postgres if input.analyze => {
                        format!("EXPLAIN (ANALYZE, FORMAT TEXT) {}", input.sql)
                    }
                    Dialect::Postgres => format!("EXPLAIN (FORMAT TEXT) {}", input.sql),
                    Dialect::Mysql if input.analyze => {
                        format!("EXPLAIN ANALYZE {}", input.sql)
                    }
                    Dialect::Mysql => format!("EXPLAIN FORMAT=TREE {}", input.sql),
                    Dialect::Sqlite => format!("EXPLAIN QUERY PLAN {}", input.sql),
                };

                match execute_query(&conn.pool, &sql).await {
                    Ok(rows) => {
                        let text = serde_json::to_string_pretty(&rows)
                            .unwrap_or_else(|_| "[]".to_string());
                        Ok(CallToolResult::text(text))
                    }
                    Err(e) => Ok(CallToolResult::error(format!("Explain failed: {e}"))),
                }
            },
        )
        .layer(TimeoutLayer::new(Duration::from_secs(default_timeout)))
        .build()
}

pub fn list_schemas_tool(db: Arc<DatabaseManager>) -> Tool {
    let output_schema =
        serde_json::to_value(schemars::schema_for!(Vec<SchemaInfo>)).unwrap_or_default();
    ToolBuilder::new("list_schemas")
        .description("List schemas (or databases for MySQL) in a connection.")
        .output_schema(output_schema)
        .extractor_handler(
            db,
            |State(db): State<Arc<DatabaseManager>>,
             Json(input): Json<DatabaseInput>| async move {
                let Some(conn) = db.get(&input.database) else {
                    return Ok(unknown_db_error(&db, &input.database));
                };

                let sql = match conn.dialect {
                    Dialect::Postgres => {
                        "SELECT schema_name::TEXT as name FROM information_schema.schemata \
                         WHERE schema_name NOT IN ('information_schema', 'pg_catalog', 'pg_toast') \
                         ORDER BY schema_name"
                    }
                    Dialect::Mysql => {
                        "SELECT CAST(schema_name AS CHAR) as name FROM information_schema.schemata \
                         WHERE schema_name NOT IN ('information_schema', 'performance_schema', 'mysql', 'sys') \
                         ORDER BY schema_name"
                    }
                    Dialect::Sqlite => {
                        // SQLite has a single implicit schema
                        return CallToolResult::from_serialize(&vec![SchemaInfo {
                            name: "main".to_string(),
                        }]);
                    }
                };

                match execute_query(&conn.pool, sql).await {
                    Ok(rows) => {
                        let schemas: Vec<SchemaInfo> = rows
                            .iter()
                            .map(|row| SchemaInfo {
                                name: row
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                            })
                            .collect();
                        CallToolResult::from_serialize(&schemas)
                    }
                    Err(e) => Ok(CallToolResult::error(format!(
                        "Failed to list schemas: {e}"
                    ))),
                }
            },
        )
        .build()
}

pub fn list_tables_tool(db: Arc<DatabaseManager>) -> Tool {
    let output_schema =
        serde_json::to_value(schemars::schema_for!(Vec<TableInfo>)).unwrap_or_default();
    ToolBuilder::new("list_tables")
        .description("List all tables in a database.")
        .output_schema(output_schema)
        .extractor_handler(
            db,
            |State(db): State<Arc<DatabaseManager>>, Json(input): Json<DatabaseInput>| async move {
                let Some(conn) = db.get(&input.database) else {
                    return Ok(unknown_db_error(&db, &input.database));
                };

                let sql = match conn.dialect {
                    Dialect::Sqlite => "SELECT name, 'table' as type FROM sqlite_master \
                         WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
                        .to_string(),
                    Dialect::Postgres => {
                        "SELECT table_name::TEXT as name, table_schema::TEXT as schema, table_type::TEXT as type \
                         FROM information_schema.tables \
                         WHERE table_schema NOT IN ('information_schema', 'pg_catalog') \
                         ORDER BY table_schema, table_name"
                            .to_string()
                    }
                    Dialect::Mysql => {
                        "SELECT CAST(table_name AS CHAR) as name, CAST(table_schema AS CHAR) as `schema`, CAST(table_type AS CHAR) as `type` \
                         FROM information_schema.tables \
                         WHERE table_schema = DATABASE() \
                         ORDER BY table_name"
                            .to_string()
                    }
                };

                match execute_query(&conn.pool, &sql).await {
                    Ok(rows) => {
                        let tables: Vec<TableInfo> = rows
                            .iter()
                            .map(|row| TableInfo {
                                name: row
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                schema: row
                                    .get("schema")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                                table_type: row
                                    .get("type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("table")
                                    .to_string(),
                            })
                            .collect();
                        CallToolResult::from_serialize(&tables)
                    }
                    Err(e) => Ok(CallToolResult::error(format!("Failed to list tables: {e}"))),
                }
            },
        )
        .build()
}

pub fn describe_table_tool(db: Arc<DatabaseManager>) -> Tool {
    let output_schema =
        serde_json::to_value(schemars::schema_for!(TableDescription)).unwrap_or_default();
    ToolBuilder::new("describe_table")
        .description("Show columns, types, constraints, and indexes for a table.")
        .output_schema(output_schema)
        .extractor_handler(
            db,
            |State(db): State<Arc<DatabaseManager>>,
             Json(input): Json<TableInput>| async move {
                let Some(conn) = db.get(&input.database) else {
                    return Ok(unknown_db_error(&db, &input.database));
                };

                // --- Columns ---
                let col_sql = match conn.dialect {
                    Dialect::Sqlite => format!("PRAGMA table_info('{}')", input.table),
                    Dialect::Postgres => {
                        let schema = input.schema.as_deref().unwrap_or("public");
                        format!(
                            "SELECT column_name::TEXT, data_type::TEXT, is_nullable::TEXT, column_default::TEXT \
                             FROM information_schema.columns \
                             WHERE table_schema = '{schema}' AND table_name = '{}' \
                             ORDER BY ordinal_position",
                            input.table
                        )
                    }
                    Dialect::Mysql => format!(
                        "SELECT CAST(column_name AS CHAR) as column_name, CAST(data_type AS CHAR) as data_type, \
                         CAST(is_nullable AS CHAR) as is_nullable, CAST(column_default AS CHAR) as column_default \
                         FROM information_schema.columns \
                         WHERE table_schema = DATABASE() AND table_name = '{}' \
                         ORDER BY ordinal_position",
                        input.table
                    ),
                };

                let columns = match execute_query(&conn.pool, &col_sql).await {
                    Ok(rows) => rows
                        .iter()
                        .map(|row| match conn.dialect {
                            Dialect::Sqlite => ColumnInfo {
                                name: json_str(row, "name"),
                                data_type: json_str(row, "type"),
                                nullable: row
                                    .get("notnull")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0)
                                    == 0,
                                default_value: row
                                    .get("dflt_value")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                            },
                            _ => ColumnInfo {
                                name: json_str(row, "column_name"),
                                data_type: json_str(row, "data_type"),
                                nullable: row
                                    .get("is_nullable")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("YES")
                                    == "YES",
                                default_value: row
                                    .get("column_default")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                            },
                        })
                        .collect(),
                    Err(e) => {
                        return Ok(CallToolResult::error(format!(
                            "Failed to describe columns: {e}"
                        )));
                    }
                };

                // --- Indexes ---
                let idx_sql = match conn.dialect {
                    Dialect::Sqlite => format!(
                        "SELECT il.name, il.\"unique\" as is_unique, group_concat(ii.name) as columns \
                         FROM pragma_index_list('{}') il \
                         LEFT JOIN pragma_index_info(il.name) ii ON true \
                         GROUP BY il.name, il.\"unique\"",
                        input.table
                    ),
                    Dialect::Postgres => {
                        let schema = input.schema.as_deref().unwrap_or("public");
                        format!(
                            "SELECT indexname::TEXT as name, indexdef::TEXT as columns \
                             FROM pg_indexes \
                             WHERE schemaname = '{schema}' AND tablename = '{}' \
                             ORDER BY indexname",
                            input.table
                        )
                    }
                    Dialect::Mysql => format!(
                        "SELECT index_name as name, GROUP_CONCAT(column_name ORDER BY seq_in_index) as columns, \
                         CASE WHEN non_unique = 0 THEN 'YES' ELSE 'NO' END as is_unique \
                         FROM information_schema.statistics \
                         WHERE table_schema = DATABASE() AND table_name = '{}' \
                         GROUP BY index_name, non_unique \
                         ORDER BY index_name",
                        input.table
                    ),
                };

                let indexes = match execute_query(&conn.pool, &idx_sql).await {
                    Ok(rows) => rows
                        .iter()
                        .map(|row| IndexInfo {
                            name: json_str(row, "name"),
                            table: input.table.clone(),
                            columns: row
                                .get("columns")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            unique: match conn.dialect {
                                Dialect::Sqlite => {
                                    row.get("is_unique").and_then(|v| v.as_i64()).unwrap_or(0) == 1
                                }
                                Dialect::Mysql => {
                                    row.get("is_unique").and_then(|v| v.as_str()) == Some("YES")
                                }
                                Dialect::Postgres => json_str(row, "columns").contains("UNIQUE"),
                            },
                        })
                        .collect(),
                    Err(_) => vec![], // Indexes are best-effort
                };

                CallToolResult::from_serialize(&TableDescription {
                    table: input.table,
                    columns,
                    indexes,
                })
            },
        )
        .build()
}

pub fn list_indexes_tool(db: Arc<DatabaseManager>) -> Tool {
    let output_schema =
        serde_json::to_value(schemars::schema_for!(Vec<IndexInfo>)).unwrap_or_default();
    ToolBuilder::new("list_indexes")
        .description("List indexes on a table, or all indexes in the database.")
        .output_schema(output_schema)
        .extractor_handler(
            db,
            |State(db): State<Arc<DatabaseManager>>,
             Json(input): Json<TableInput>| async move {
                let Some(conn) = db.get(&input.database) else {
                    return Ok(unknown_db_error(&db, &input.database));
                };

                let sql = match conn.dialect {
                    Dialect::Sqlite => format!(
                        "SELECT il.name, m.name as tbl, il.\"unique\" as is_unique \
                         FROM sqlite_master m, pragma_index_list(m.name) il \
                         WHERE m.type = 'table' AND m.name LIKE '{}' \
                         ORDER BY m.name, il.name",
                        if input.table.is_empty() { "%" } else { &input.table }
                    ),
                    Dialect::Postgres => {
                        let schema = input.schema.as_deref().unwrap_or("public");
                        if input.table.is_empty() {
                            format!(
                                "SELECT indexname::TEXT as name, tablename::TEXT as tbl, indexdef::TEXT as columns \
                                 FROM pg_indexes WHERE schemaname = '{schema}' ORDER BY tablename, indexname"
                            )
                        } else {
                            format!(
                                "SELECT indexname::TEXT as name, tablename::TEXT as tbl, indexdef::TEXT as columns \
                                 FROM pg_indexes WHERE schemaname = '{schema}' AND tablename = '{}' \
                                 ORDER BY indexname",
                                input.table
                            )
                        }
                    }
                    Dialect::Mysql => {
                        if input.table.is_empty() {
                            "SELECT index_name as name, table_name as tbl, \
                             GROUP_CONCAT(column_name ORDER BY seq_in_index) as columns, \
                             CASE WHEN non_unique = 0 THEN 1 ELSE 0 END as is_unique \
                             FROM information_schema.statistics \
                             WHERE table_schema = DATABASE() \
                             GROUP BY index_name, table_name, non_unique \
                             ORDER BY table_name, index_name"
                                .to_string()
                        } else {
                            format!(
                                "SELECT index_name as name, table_name as tbl, \
                                 GROUP_CONCAT(column_name ORDER BY seq_in_index) as columns, \
                                 CASE WHEN non_unique = 0 THEN 1 ELSE 0 END as is_unique \
                                 FROM information_schema.statistics \
                                 WHERE table_schema = DATABASE() AND table_name = '{}' \
                                 GROUP BY index_name, table_name, non_unique \
                                 ORDER BY index_name",
                                input.table
                            )
                        }
                    }
                };

                match execute_query(&conn.pool, &sql).await {
                    Ok(rows) => {
                        let indexes: Vec<IndexInfo> = rows
                            .iter()
                            .map(|row| IndexInfo {
                                name: json_str(row, "name"),
                                table: json_str(row, "tbl"),
                                columns: row
                                    .get("columns")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                                unique: row
                                    .get("is_unique")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0)
                                    == 1,
                            })
                            .collect();
                        CallToolResult::from_serialize(&indexes)
                    }
                    Err(e) => Ok(CallToolResult::error(format!(
                        "Failed to list indexes: {e}"
                    ))),
                }
            },
        )
        .build()
}

pub fn list_connections_tool(db: Arc<DatabaseManager>) -> Tool {
    let output_schema =
        serde_json::to_value(schemars::schema_for!(Vec<ConnectionInfo>)).unwrap_or_default();
    ToolBuilder::new("list_connections")
        .description("List all configured database connections with their settings and status.")
        .output_schema(output_schema)
        .extractor_handler(db, |State(db): State<Arc<DatabaseManager>>| async move {
            let conns: Vec<ConnectionInfo> = db
                .names()
                .iter()
                .map(|name| {
                    let conn = db.get(name).unwrap();
                    ConnectionInfo {
                        name: name.clone(),
                        dialect: format!("{:?}", conn.dialect).to_lowercase(),
                        read_only: conn.read_only,
                        row_limit: conn.row_limit,
                        query_timeout_seconds: conn.query_timeout_seconds,
                        connected: true,
                    }
                })
                .collect();
            CallToolResult::from_serialize(&conns)
        })
        .build()
}

pub fn test_connection_tool(db: Arc<DatabaseManager>) -> Tool {
    ToolBuilder::new("test_connection")
        .description("Test database connectivity and report latency.")
        .extractor_handler(
            db,
            |State(db): State<Arc<DatabaseManager>>, Json(input): Json<DatabaseInput>| async move {
                let Some(conn) = db.get(&input.database) else {
                    return Ok(unknown_db_error(&db, &input.database));
                };

                let start = std::time::Instant::now();
                let test_sql = match conn.dialect {
                    Dialect::Sqlite | Dialect::Postgres => "SELECT 1",
                    Dialect::Mysql => "SELECT 1",
                };

                match execute_query(&conn.pool, test_sql).await {
                    Ok(_) => {
                        let elapsed = start.elapsed();
                        Ok(CallToolResult::text(format!(
                            "Connection '{}' ({:?}): OK ({:.1}ms)",
                            input.database,
                            conn.dialect,
                            elapsed.as_secs_f64() * 1000.0
                        )))
                    }
                    Err(e) => Ok(CallToolResult::error(format!(
                        "Connection '{}' failed: {e}",
                        input.database
                    ))),
                }
            },
        )
        .build()
}

fn json_str(row: &serde_json::Value, key: &str) -> String {
    row.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}
