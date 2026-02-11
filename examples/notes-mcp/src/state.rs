//! Shared application state and data models

use redis::aio::MultiplexedConnection;
use serde::{Deserialize, Serialize};

/// A customer record stored as RedisJSON
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Customer {
    pub id: String,
    pub name: String,
    pub company: String,
    pub email: String,
    pub role: String,
    pub tier: String,
}

/// A note attached to a customer, stored as RedisJSON
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Note {
    pub id: String,
    pub customer_id: String,
    pub content: String,
    pub note_type: String,
    pub tags: Vec<String>,
    pub created_at: String,
}

/// A customer with their note count, used by list_customers JSON output
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomerSummary {
    #[serde(flatten)]
    pub customer: Customer,
    pub note_count: usize,
}

/// A customer profile bundled with their notes, used by get_customer JSON output
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomerWithNotes {
    #[serde(flatten)]
    pub customer: Customer,
    pub notes: Vec<Note>,
}

/// Returns true if the caller requested JSON output format.
pub fn is_json_output(format: &Option<String>) -> bool {
    matches!(format.as_deref(), Some("json"))
}

/// Shared state for the MCP server
pub struct AppState {
    conn: MultiplexedConnection,
}

impl AppState {
    /// Connect to Redis and return a new AppState
    pub async fn new(redis_url: &str) -> Result<Self, tower_mcp::BoxError> {
        let client = redis::Client::open(redis_url)?;
        let conn = client.get_multiplexed_async_connection().await?;
        Ok(Self { conn })
    }

    /// Get a clone of the multiplexed connection (cheap)
    pub fn conn(&self) -> MultiplexedConnection {
        self.conn.clone()
    }
}

/// Escape special characters for RediSearch TAG filter values.
///
/// RediSearch interprets characters like `-`, `.`, `@`, etc. as operators.
/// Tags such as `case-study` or `disaster-recovery` must be escaped to match.
pub fn escape_tag(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() * 2);
    for ch in value.chars() {
        if matches!(
            ch,
            ',' | '.'
                | '<'
                | '>'
                | '{'
                | '}'
                | '['
                | ']'
                | '"'
                | '\''
                | ':'
                | ';'
                | '!'
                | '@'
                | '#'
                | '$'
                | '%'
                | '^'
                | '&'
                | '*'
                | '('
                | ')'
                | '-'
                | '+'
                | '='
                | '~'
                | '/'
                | '|'
                | ' '
                | '\\'
        ) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Transform a multi-word text query into an OR query for RediSearch.
///
/// RediSearch ANDs all terms by default, so `"case study marketing"` returns
/// nothing unless a document contains all three words. Joining with `|` makes
/// it an OR so any matching term produces results.
pub fn or_join_query(query: &str) -> String {
    let trimmed = query.trim();
    // Don't transform single words, wildcard, or queries already using RediSearch operators
    if !trimmed.contains(' ')
        || trimmed == "*"
        || trimmed.contains('|')
        || trimmed.contains('@')
        || trimmed.contains('(')
    {
        return trimmed.to_string();
    }
    let terms: Vec<&str> = trimmed.split_whitespace().collect();
    format!("({})", terms.join("|"))
}

/// Parse an `FT.SEARCH` response that used `RETURN 1 $`.
///
/// Returns `(total_count, vec_of_(key, json_string))`.
pub fn parse_ft_search(
    values: Vec<redis::Value>,
) -> Result<(usize, Vec<(String, String)>), tower_mcp::BoxError> {
    // FT.SEARCH returns: [total_count, key1, [field, value], key2, [field, value], ...]
    let mut iter = values.into_iter();

    let total: usize = match iter.next() {
        Some(redis::Value::Int(n)) => n as usize,
        _ => return Err("Expected integer total count from FT.SEARCH".into()),
    };

    let mut results = Vec::new();
    while let Some(key_val) = iter.next() {
        let key: String = redis::FromRedisValue::from_redis_value(key_val)?;
        let fields_val = iter
            .next()
            .ok_or("Expected fields after key in FT.SEARCH result")?;

        // Fields come as a flat list: [field_name, field_value, ...]
        let fields: Vec<String> = redis::FromRedisValue::from_redis_value(fields_val)?;

        // We expect pairs; find the "$" field and grab its value
        let json_str = fields
            .chunks(2)
            .find(|pair| pair.len() == 2 && pair[0] == "$")
            .map(|pair| pair[1].clone())
            .unwrap_or_default();

        results.push((key, json_str));
    }

    Ok((total, results))
}
