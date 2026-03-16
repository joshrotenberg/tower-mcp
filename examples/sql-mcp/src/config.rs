use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub safety: SafetyConfig,
    pub databases: Vec<DatabaseConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            name: default_name(),
            version: default_version(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SafetyConfig {
    /// Default read-only mode (can be overridden per-database)
    #[serde(default = "default_true")]
    pub read_only: bool,
    /// Default row limit for queries
    #[serde(default = "default_row_limit")]
    pub row_limit: u32,
    /// Default query timeout in seconds
    #[serde(default = "default_timeout")]
    pub query_timeout_seconds: u64,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            read_only: true,
            row_limit: default_row_limit(),
            query_timeout_seconds: default_timeout(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DatabaseConfig {
    /// Connection name (used in tool calls)
    pub name: String,
    /// Connection URL (supports ${ENV_VAR} expansion)
    pub url: String,
    /// Override read-only setting for this database
    pub read_only: Option<bool>,
    /// Override row limit for this database
    pub row_limit: Option<u32>,
    /// Override query timeout for this database
    pub query_timeout_seconds: Option<u64>,
    /// Connection pool size
    #[serde(default = "default_pool_size")]
    pub pool_size: u32,
}

impl DatabaseConfig {
    pub fn is_read_only(&self, global: &SafetyConfig) -> bool {
        self.read_only.unwrap_or(global.read_only)
    }

    pub fn row_limit(&self, global: &SafetyConfig) -> u32 {
        self.row_limit.unwrap_or(global.row_limit)
    }

    pub fn query_timeout_seconds(&self, global: &SafetyConfig) -> u64 {
        self.query_timeout_seconds
            .unwrap_or(global.query_timeout_seconds)
    }

    /// Resolve ${ENV_VAR} references in the URL.
    pub fn resolved_url(&self) -> String {
        let mut url = self.url.clone();
        while let Some(start) = url.find("${") {
            if let Some(end) = url[start..].find('}') {
                let var_name = &url[start + 2..start + end];
                let value = std::env::var(var_name).unwrap_or_default();
                url = format!("{}{}{}", &url[..start], value, &url[start + end + 1..]);
            } else {
                break;
            }
        }
        url
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Config =
            toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;
        anyhow::ensure!(!config.databases.is_empty(), "no databases configured");
        Ok(config)
    }
}

fn default_name() -> String {
    "sql-mcp".to_string()
}
fn default_version() -> String {
    "0.1.0".to_string()
}
fn default_true() -> bool {
    true
}
fn default_row_limit() -> u32 {
    1000
}
fn default_timeout() -> u64 {
    30
}
fn default_pool_size() -> u32 {
    5
}
