use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use sqlx::any::AnyPoolOptions;
use sqlx::{AnyPool, Column, Row, TypeInfo, ValueRef};

use crate::config::{Config, DatabaseConfig, SafetyConfig};

/// Manages database connection pools.
pub struct DatabaseManager {
    pools: HashMap<String, DatabaseConnection>,
}

/// Database dialect, detected from the connection URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    Mysql,
    Sqlite,
}

/// A single database connection with its resolved config.
pub struct DatabaseConnection {
    pub pool: AnyPool,
    pub dialect: Dialect,
    pub read_only: bool,
    pub row_limit: u32,
    pub query_timeout_seconds: u64,
}

impl DatabaseManager {
    pub async fn from_config(config: &Config) -> Result<Arc<Self>> {
        sqlx::any::install_default_drivers();

        let mut pools = HashMap::new();

        for db_config in &config.databases {
            let conn = Self::connect(db_config, &config.safety).await?;
            pools.insert(db_config.name.clone(), conn);
        }

        Ok(Arc::new(Self { pools }))
    }

    async fn connect(
        db_config: &DatabaseConfig,
        safety: &SafetyConfig,
    ) -> Result<DatabaseConnection> {
        let url = db_config.resolved_url();
        let dialect = Dialect::from_url(&url);
        let pool = AnyPoolOptions::new()
            .max_connections(db_config.pool_size)
            .connect(&url)
            .await
            .with_context(|| format!("connecting to database '{}'", db_config.name))?;

        Ok(DatabaseConnection {
            pool,
            dialect,
            read_only: db_config.is_read_only(safety),
            row_limit: db_config.row_limit(safety),
            query_timeout_seconds: db_config.query_timeout_seconds(safety),
        })
    }

    pub fn get(&self, name: &str) -> Option<&DatabaseConnection> {
        self.pools.get(name)
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.pools.keys().cloned().collect();
        names.sort();
        names
    }
}

impl Dialect {
    pub fn from_url(url: &str) -> Self {
        if url.starts_with("sqlite") {
            Dialect::Sqlite
        } else if url.starts_with("mysql") || url.starts_with("mariadb") {
            Dialect::Mysql
        } else {
            Dialect::Postgres
        }
    }
}

/// Execute a query and return results as a Vec of JSON objects.
pub async fn execute_query(pool: &AnyPool, sql: &str) -> Result<Vec<serde_json::Value>> {
    let rows: Vec<sqlx::any::AnyRow> = sqlx::query(sql).fetch_all(pool).await?;

    let mut results = Vec::with_capacity(rows.len());
    for row in &rows {
        let columns = row.columns();
        let mut obj = serde_json::Map::new();
        for col in columns {
            let name = col.name().to_string();
            let value = row_value_to_json(row, col);
            obj.insert(name, value);
        }
        results.push(serde_json::Value::Object(obj));
    }

    Ok(results)
}

fn row_value_to_json(
    row: &sqlx::any::AnyRow,
    col: &<sqlx::Any as sqlx::Database>::Column,
) -> serde_json::Value {
    // Check for NULL first
    if row.try_get_raw(col.ordinal()).map_or(true, |v| v.is_null()) {
        return serde_json::Value::Null;
    }

    let type_name = col.type_info().name();

    match type_name {
        "INTEGER" | "INT4" | "INT8" | "BIGINT" | "SMALLINT" | "INT" | "INT2" => row
            .try_get::<i64, _>(col.ordinal())
            .map(|v| serde_json::Value::Number(v.into()))
            .unwrap_or(serde_json::Value::Null),
        "REAL" | "FLOAT4" | "FLOAT8" | "DOUBLE" | "DOUBLE PRECISION" | "NUMERIC" => row
            .try_get::<f64, _>(col.ordinal())
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        "BOOLEAN" | "BOOL" => row
            .try_get::<bool, _>(col.ordinal())
            .map(serde_json::Value::Bool)
            .unwrap_or(serde_json::Value::Null),
        _ => row
            .try_get::<String, _>(col.ordinal())
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null),
    }
}
