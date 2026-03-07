---
name: rust-error-handling
description: Rust error handling best practices and patterns
---

Guide for implementing error handling in Rust projects.

## Library vs Application

- **Libraries**: Use `thiserror` for typed, structured errors. Each error variant should be meaningful to callers.
- **Applications**: Use `anyhow` for ergonomic error propagation. Context via `.context()` and `.with_context()`.
- **Never** use `unwrap()` or `expect()` in library code unless the invariant is provably guaranteed.

## Error Design

```rust
// Good: specific, actionable error types
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file not found: {path}")]
    NotFound { path: PathBuf },
    #[error("invalid config at line {line}: {reason}")]
    Parse { line: usize, reason: String },
}

// Bad: stringly-typed errors
type Result<T> = std::result::Result<T, String>;
```

## The `?` Operator

Use `?` for propagation. Add context at boundaries where the original error loses meaning:

```rust
let config = fs::read_to_string(&path)
    .with_context(|| format!("failed to read config from {}", path.display()))?;
```

## When to Panic

Only panic for programmer errors (logic bugs), never for runtime errors (I/O, network, user input).
