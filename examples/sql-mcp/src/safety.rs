use sqlparser::ast::Statement;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

/// Parse SQL and validate it against safety rules.
/// Returns an error message if the SQL is not allowed.
pub fn validate_read_only(sql: &str) -> Result<(), String> {
    let dialect = GenericDialect {};
    let statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| format!("SQL parse error: {e}"))?;

    if statements.is_empty() {
        return Err("Empty SQL statement".to_string());
    }

    if statements.len() > 1 {
        return Err("Multiple statements not allowed".to_string());
    }

    match &statements[0] {
        Statement::Query(_)
        | Statement::ExplainTable { .. }
        | Statement::Explain { .. }
        | Statement::ShowTables { .. }
        | Statement::ShowColumns { .. } => Ok(()),
        stmt => Err(format!(
            "Statement type not allowed in read-only mode: {}",
            short_statement_name(stmt)
        )),
    }
}

/// Inject a LIMIT clause if none is present.
/// Uses simple string matching to avoid the complexity of AST manipulation.
pub fn enforce_row_limit(sql: &str, max_rows: u32) -> String {
    let upper = sql.to_uppercase();
    // Only add LIMIT to SELECT queries that don't already have one
    if upper.trim_start().starts_with("SELECT") && !upper.contains("LIMIT") {
        format!("{sql} LIMIT {max_rows}")
    } else {
        sql.to_string()
    }
}

fn short_statement_name(stmt: &Statement) -> &'static str {
    match stmt {
        Statement::Insert(_) => "INSERT",
        Statement::Update { .. } => "UPDATE",
        Statement::Delete(_) => "DELETE",
        Statement::Drop { .. } => "DROP",
        Statement::CreateTable(_) => "CREATE TABLE",
        Statement::CreateIndex(_) => "CREATE INDEX",
        Statement::AlterTable { .. } => "ALTER TABLE",
        Statement::Truncate { .. } => "TRUNCATE",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_select() {
        assert!(validate_read_only("SELECT * FROM users").is_ok());
    }

    #[test]
    fn allows_explain() {
        assert!(validate_read_only("EXPLAIN SELECT * FROM users").is_ok());
    }

    #[test]
    fn rejects_insert() {
        assert!(validate_read_only("INSERT INTO users VALUES (1, 'a')").is_err());
    }

    #[test]
    fn rejects_update() {
        assert!(validate_read_only("UPDATE users SET name = 'a'").is_err());
    }

    #[test]
    fn rejects_delete() {
        assert!(validate_read_only("DELETE FROM users").is_err());
    }

    #[test]
    fn rejects_drop() {
        assert!(validate_read_only("DROP TABLE users").is_err());
    }

    #[test]
    fn rejects_multiple_statements() {
        assert!(validate_read_only("SELECT 1; DROP TABLE users").is_err());
    }

    #[test]
    fn enforces_limit_when_missing() {
        let result = enforce_row_limit("SELECT * FROM users", 100);
        assert!(result.contains("LIMIT 100"));
    }

    #[test]
    fn preserves_existing_limit_within_bounds() {
        let sql = "SELECT * FROM users LIMIT 50";
        let result = enforce_row_limit(sql, 100);
        assert_eq!(result, sql);
    }
}
