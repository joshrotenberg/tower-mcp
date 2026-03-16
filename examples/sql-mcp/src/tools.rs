use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tower::timeout::TimeoutLayer;
use tower_mcp::extract::{Json, State};
use tower_mcp::tool::Tool;
use tower_mcp::{CallToolResult, ToolBuilder};

use crate::db::{DatabaseManager, execute_query};
use crate::safety;

// --- Input types ---

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryInput {
    /// Database connection name
    pub database: String,
    /// SQL query to execute
    pub sql: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTablesInput {
    /// Database connection name
    pub database: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeTableInput {
    /// Database connection name
    pub database: String,
    /// Table name to describe
    pub table: String,
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
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TableDescription {
    pub table: String,
    pub columns: Vec<ColumnInfo>,
}

// --- Tool builders ---

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
                    return Ok(CallToolResult::error(format!(
                        "Unknown database '{}'. Available: {}",
                        input.database,
                        db.names().join(", ")
                    )));
                };

                // Validate read-only
                if conn.read_only
                    && let Err(msg) = safety::validate_read_only(&input.sql)
                {
                    return Ok(CallToolResult::error(msg));
                }

                // Enforce row limit
                let sql = safety::enforce_row_limit(&input.sql, conn.row_limit);

                // Execute
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

pub fn list_tables_tool(db: Arc<DatabaseManager>) -> Tool {
    let output_schema =
        serde_json::to_value(schemars::schema_for!(Vec<TableInfo>)).unwrap_or_default();
    ToolBuilder::new("list_tables")
        .description("List all tables in a database.")
        .output_schema(output_schema)
        .extractor_handler(
            db,
            |State(db): State<Arc<DatabaseManager>>,
             Json(input): Json<ListTablesInput>| async move {
                let Some(conn) = db.get(&input.database) else {
                    return Ok(CallToolResult::error(format!(
                        "Unknown database '{}'. Available: {}",
                        input.database,
                        db.names().join(", ")
                    )));
                };

                let sql = if DatabaseManager::is_sqlite(&conn.pool) {
                    "SELECT name, 'table' as type FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
                } else {
                    "SELECT table_name as name, table_schema as schema, table_type as type FROM information_schema.tables WHERE table_schema NOT IN ('information_schema', 'pg_catalog') ORDER BY table_schema, table_name"
                };

                match execute_query(&conn.pool, sql).await {
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
        .description("Show columns, types, and constraints for a table.")
        .output_schema(output_schema)
        .extractor_handler(
            db,
            |State(db): State<Arc<DatabaseManager>>,
             Json(input): Json<DescribeTableInput>| async move {
                let Some(conn) = db.get(&input.database) else {
                    return Ok(CallToolResult::error(format!(
                        "Unknown database '{}'. Available: {}",
                        input.database,
                        db.names().join(", ")
                    )));
                };

                let is_sqlite = DatabaseManager::is_sqlite(&conn.pool);
                let sql = if is_sqlite {
                    format!("PRAGMA table_info('{}')", input.table)
                } else {
                    format!(
                        "SELECT column_name, data_type, is_nullable, column_default \
                         FROM information_schema.columns \
                         WHERE table_name = '{}' \
                         ORDER BY ordinal_position",
                        input.table
                    )
                };

                match execute_query(&conn.pool, &sql).await {
                    Ok(rows) => {
                        let columns: Vec<ColumnInfo> = rows
                            .iter()
                            .map(|row| {
                                if is_sqlite {
                                    ColumnInfo {
                                        name: row
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        data_type: row
                                            .get("type")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        nullable: row
                                            .get("notnull")
                                            .and_then(|v| v.as_i64())
                                            .unwrap_or(0)
                                            == 0,
                                        default_value: row
                                            .get("dflt_value")
                                            .and_then(|v| v.as_str())
                                            .map(String::from),
                                    }
                                } else {
                                    ColumnInfo {
                                        name: row
                                            .get("column_name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        data_type: row
                                            .get("data_type")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        nullable: row
                                            .get("is_nullable")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("YES")
                                            == "YES",
                                        default_value: row
                                            .get("column_default")
                                            .and_then(|v| v.as_str())
                                            .map(String::from),
                                    }
                                }
                            })
                            .collect();

                        CallToolResult::from_serialize(&TableDescription {
                            table: input.table,
                            columns,
                        })
                    }
                    Err(e) => Ok(CallToolResult::error(format!(
                        "Failed to describe table: {e}"
                    ))),
                }
            },
        )
        .build()
}
