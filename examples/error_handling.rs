//! Error handling patterns example
//!
//! Demonstrates the different ways to handle errors in tower-mcp tool handlers.
//! Understanding when to use each pattern is important for building robust servers.
//!
//! Key distinction:
//! - `Ok(CallToolResult::error(...))` -- Tool execution error. The LLM sees this
//!   message and can self-correct (e.g., fix invalid arguments, try a different approach).
//! - `Err(...)` -- Handler failure. Automatically converted to `CallToolResult::error()`
//!   by tower-mcp, so the LLM still sees it. Use for unexpected/unrecoverable errors.
//!
//! Both approaches result in `isError: true` in the response -- the difference is
//! ergonomic. Use `Ok(CallToolResult::error(...))` for expected/domain errors where
//! you want explicit control over the message. Use `Err(...)` with `?` for propagating
//! unexpected failures.
//!
//! Run with: cargo run --example error_handling
//!
//! Test interactively or with an MCP client connected via stdio.

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{BoxError, CallToolResult, McpRouter, StdioTransport, ToolBuilder};

#[derive(Debug, Deserialize, JsonSchema)]
struct DivideInput {
    /// Numerator
    a: f64,
    /// Denominator
    b: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FetchInput {
    /// URL to fetch (simulated)
    url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ParseInput {
    /// JSON string to parse
    json_text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ValidatedInput {
    /// Email address (must contain @)
    email: String,
    /// Age (must be 0-150)
    age: u32,
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    eprintln!("Starting error handling example...");

    // Pattern 1: Domain validation errors
    // Use CallToolResult::error() for expected, recoverable errors.
    // The LLM can read the message and adjust its input.
    let divide = ToolBuilder::new("divide")
        .description("Divide two numbers. Returns an error for division by zero.")
        .handler(|input: DivideInput| async move {
            if input.b == 0.0 {
                // Expected domain error -- the LLM can fix this
                return Ok(CallToolResult::error(
                    "Division by zero. The denominator must be non-zero.",
                ));
            }
            Ok(CallToolResult::text(format!("{}", input.a / input.b)))
        })
        .build();

    // Pattern 2: Input validation with multiple checks
    // Validate all fields and return a combined error message.
    let validate_user = ToolBuilder::new("validate_user")
        .description("Validate user input. Email must contain @, age must be 0-150.")
        .handler(|input: ValidatedInput| async move {
            let mut errors = Vec::new();

            if !input.email.contains('@') {
                errors.push(format!("Invalid email '{}': must contain @", input.email));
            }
            if input.age > 150 {
                errors.push(format!("Invalid age {}: must be 0-150", input.age));
            }

            if !errors.is_empty() {
                return Ok(CallToolResult::error(errors.join("; ")));
            }

            Ok(CallToolResult::text(format!(
                "Valid: {} (age {})",
                input.email, input.age
            )))
        })
        .build();

    // Pattern 3: Propagating errors with ?
    // Use Err() for unexpected failures. tower-mcp automatically converts
    // these to CallToolResult::error() so the LLM still sees the message.
    let parse_json = ToolBuilder::new("parse_json")
        .description("Parse a JSON string and pretty-print it. Returns error on invalid JSON.")
        .handler(|input: ParseInput| async move {
            // The ? operator propagates serde errors, which tower-mcp
            // converts to CallToolResult::error() automatically
            let value: serde_json::Value = serde_json::from_str(&input.json_text)?;
            let pretty = serde_json::to_string_pretty(&value)?;
            Ok(CallToolResult::text(pretty))
        })
        .build();

    // Pattern 4: Mixed error handling -- domain errors + unexpected failures
    // Use explicit error for known cases, ? for unexpected ones.
    let fetch_url = ToolBuilder::new("fetch_url")
        .description("Simulate fetching a URL. Certain URLs return known errors.")
        .handler(|input: FetchInput| async move {
            // Known domain errors -- explicit messages help the LLM
            if input.url.is_empty() {
                return Ok(CallToolResult::error("URL cannot be empty"));
            }
            if !input.url.starts_with("https://") {
                return Ok(CallToolResult::error(format!(
                    "URL must start with https://, got: {}",
                    input.url
                )));
            }

            // Simulate different responses
            match input.url.as_str() {
                "https://example.com/404" => Ok(CallToolResult::error("HTTP 404: page not found")),
                "https://example.com/500" => Ok(CallToolResult::error(
                    "HTTP 500: internal server error. Try again later.",
                )),
                url => Ok(CallToolResult::text(format!(
                    "Successfully fetched {} (200 OK, 1234 bytes)",
                    url
                ))),
            }
        })
        .build();

    let router = McpRouter::new()
        .server_info("error-handling-example", "1.0.0")
        .instructions(
            "This server demonstrates error handling patterns. Try:\n\
             - divide: try b=0 to see domain validation error\n\
             - validate_user: try invalid email or age>150\n\
             - parse_json: try invalid JSON to see propagated error\n\
             - fetch_url: try empty, non-https, or /404 /500 paths",
        )
        .tool(divide)
        .tool(validate_user)
        .tool(parse_json)
        .tool(fetch_url);

    eprintln!("Server ready. Connect with an MCP client via stdio.");
    StdioTransport::new(router).run().await?;

    Ok(())
}
