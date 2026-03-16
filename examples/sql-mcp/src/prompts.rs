use std::sync::Arc;

use tower_mcp::prompt::{Prompt, PromptBuilder};
use tower_mcp::protocol::Content;
use tower_mcp::{GetPromptResult, PromptMessage, PromptRole};

use crate::db::{DatabaseManager, Dialect, execute_query};

pub fn sql_assistant_prompt(db: Arc<DatabaseManager>) -> Prompt {
    PromptBuilder::new("sql_assistant")
        .description(
            "Prime the conversation with schema context and safety rules for a database. \
             Reduces back-and-forth by giving the LLM everything it needs upfront.",
        )
        .required_arg("database", "Database connection name")
        .optional_arg("task", "What you want to accomplish")
        .handler(move |args| {
            let db = db.clone();
            async move {
                let db_name = args
                    .get("database")
                    .cloned()
                    .unwrap_or_else(|| "default".to_string());
                let task = args.get("task").cloned().unwrap_or_default();

                let Some(conn) = db.get(&db_name) else {
                    return Ok(GetPromptResult {
                        description: None,
                        messages: vec![PromptMessage {
                            role: PromptRole::User,
                            content: Content::Text {
                                text: format!(
                                    "Unknown database '{}'. Available: {}",
                                    db_name,
                                    db.names().join(", ")
                                ),
                                annotations: None,
                                meta: None,
                            },
                            meta: None,
                        }],
                        meta: None,
                    });
                };

                // Gather schema context
                let tables_sql = match conn.dialect {
                    Dialect::Sqlite => {
                        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name".to_string()
                    }
                    Dialect::Postgres => {
                        "SELECT table_schema || '.' || table_name as name FROM information_schema.tables WHERE table_schema NOT IN ('information_schema', 'pg_catalog') ORDER BY table_schema, table_name".to_string()
                    }
                    Dialect::Mysql => {
                        "SELECT table_name as name FROM information_schema.tables WHERE table_schema = DATABASE() ORDER BY table_name".to_string()
                    }
                };

                let table_list = match execute_query(&conn.pool, &tables_sql).await {
                    Ok(rows) => rows
                        .iter()
                        .filter_map(|r| r.get("name").and_then(|v| v.as_str()))
                        .collect::<Vec<_>>()
                        .join(", "),
                    Err(_) => "(unable to list tables)".to_string(),
                };

                let mode = if conn.read_only {
                    "READ-ONLY. Only SELECT, EXPLAIN, SHOW, and DESCRIBE are allowed."
                } else {
                    "READ-WRITE. Mutations (INSERT, UPDATE, DELETE) are permitted."
                };

                let mut prompt = format!(
                    "You are a SQL assistant connected to the '{db_name}' database.\n\n\
                     ## Connection Details\n\
                     - Dialect: {:?}\n\
                     - Mode: {mode}\n\
                     - Row limit: {} (automatically enforced)\n\
                     - Query timeout: {}s\n\n\
                     ## Available Tables\n\
                     {table_list}\n\n\
                     ## Rules\n\
                     - Use the `query` tool with `database: \"{db_name}\"` to execute SQL\n\
                     - Use `describe_table` to inspect column details before writing queries\n\
                     - Use `explain` to check query plans for complex queries\n\
                     - Row limits are enforced automatically; you don't need to add LIMIT\n\
                     - Always use parameterized queries when incorporating user-provided values\n",
                    conn.dialect, conn.row_limit, conn.query_timeout_seconds,
                );

                if !task.is_empty() {
                    prompt.push_str(&format!("\n## Task\n{task}\n"));
                }

                Ok(GetPromptResult {
                    description: Some(format!("SQL assistant for '{db_name}'")),
                    messages: vec![PromptMessage {
                        role: PromptRole::User,
                        content: Content::Text {
                            text: prompt,
                            annotations: None,
                            meta: None,
                        },
                        meta: None,
                    }],
                    meta: None,
                })
            }
        })
        .build()
}
