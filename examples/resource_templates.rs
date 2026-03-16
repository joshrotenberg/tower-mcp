//! Resource template example
//!
//! Demonstrates parameterized resources using URI templates (RFC 6570).
//! When a client requests `resources/read` with a URI matching a template,
//! the server extracts the variables and passes them to the handler.
//!
//! This example shows:
//! - Simple templates: `db://users/{id}`
//! - Multi-variable templates: `api://v1/{service}/{resource}/{id}`
//! - Wildcard path templates: `file:///{+path}` (matches slashes)
//! - MIME type hints for different content types
//! - Error handling within template handlers
//!
//! Run with: cargo run --example resource_templates
//!
//! Test interactively or with an MCP client connected via stdio.

use std::collections::HashMap;

use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
use tower_mcp::resource::ResourceTemplateBuilder;
use tower_mcp::{BoxError, McpRouter, StdioTransport};

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    eprintln!("Starting resource templates example...");

    // Template 1: Simple single-variable template
    // Matches: db://users/123, db://users/alice
    let users = ResourceTemplateBuilder::new("db://users/{id}")
        .name("User Records")
        .description("Look up user records by ID")
        .mime_type("application/json")
        .handler(|uri: String, vars: HashMap<String, String>| async move {
            let id = vars.get("id").cloned().unwrap_or_default();

            // Simulate a database lookup with error handling
            if id == "0" {
                return Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: Some("text/plain".to_string()),
                        text: Some("User not found".to_string()),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                });
            }

            let user_json = serde_json::json!({
                "id": id,
                "name": format!("User {}", id),
                "email": format!("user{}@example.com", id),
                "created_at": "2026-01-15T10:30:00Z"
            });

            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: Some("application/json".to_string()),
                    text: Some(serde_json::to_string_pretty(&user_json)?),
                    blob: None,
                    meta: None,
                }],
                meta: None,
            })
        });

    // Template 2: Multi-variable template
    // Matches: api://v1/billing/invoices/42, api://v1/auth/tokens/abc
    let api_resources = ResourceTemplateBuilder::new("api://v1/{service}/{resource}/{id}")
        .name("API Resources")
        .description("Access API resources by service, type, and ID")
        .mime_type("application/json")
        .handler(|uri: String, vars: HashMap<String, String>| async move {
            let service = vars.get("service").cloned().unwrap_or_default();
            let resource = vars.get("resource").cloned().unwrap_or_default();
            let id = vars.get("id").cloned().unwrap_or_default();

            let response = serde_json::json!({
                "service": service,
                "resource": resource,
                "id": id,
                "data": format!("{}/{}/{}", service, resource, id),
                "fetched_at": "2026-03-16T12:00:00Z"
            });

            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: Some("application/json".to_string()),
                    text: Some(serde_json::to_string_pretty(&response)?),
                    blob: None,
                    meta: None,
                }],
                meta: None,
            })
        });

    // Template 3: Wildcard path template using {+path}
    // The + prefix matches across slashes, so file:///src/main.rs works
    // Matches: file:///README.md, file:///src/lib.rs, file:///docs/guide/intro.md
    let files = ResourceTemplateBuilder::new("file:///{+path}")
        .name("Project Files")
        .description("Read project files by path (supports nested directories)")
        .handler(|uri: String, vars: HashMap<String, String>| async move {
            let path = vars.get("path").cloned().unwrap_or_default();

            // Simulate file content based on extension
            let (mime_type, content) = if path.ends_with(".json") {
                (
                    "application/json",
                    format!("{{\"file\": \"{}\", \"type\": \"json\"}}", path),
                )
            } else if path.ends_with(".md") {
                (
                    "text/markdown",
                    format!("# {}\n\nThis is the content of `{}`.", path, path),
                )
            } else {
                (
                    "text/plain",
                    format!("// Contents of {}\nfn main() {{}}", path),
                )
            };

            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: Some(mime_type.to_string()),
                    text: Some(content),
                    blob: None,
                    meta: None,
                }],
                meta: None,
            })
        });

    let router = McpRouter::new()
        .server_info("resource-templates-example", "1.0.0")
        .instructions(
            "This server demonstrates resource templates. Try reading:\n\
             - db://users/1 or db://users/alice (single variable)\n\
             - db://users/0 (returns not found)\n\
             - api://v1/billing/invoices/42 (multi-variable)\n\
             - file:///README.md or file:///src/lib.rs (wildcard path)",
        )
        .resource_template(users)
        .resource_template(api_resources)
        .resource_template(files);

    eprintln!("Server ready. Connect with an MCP client via stdio.");
    StdioTransport::new(router).run().await?;

    Ok(())
}
