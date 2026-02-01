//! Code generation from project state.
//!
//! This module generates Rust source code from the accumulated [`ProjectState`].

use crate::state::{HandlerType, ProjectState, ToolDef, Transport};

/// Generate complete Rust code from the project state.
pub fn generate_code(state: &ProjectState) -> Result<GeneratedCode, String> {
    if !state.initialized {
        return Err("Project not initialized. Call init_project first.".to_string());
    }

    let name = state.name.as_ref().ok_or("Project name not set")?;

    Ok(GeneratedCode {
        cargo_toml: generate_cargo_toml(state, name),
        main_rs: generate_main_rs(state),
        readme_md: generate_readme(state, name),
    })
}

/// The generated code files.
pub struct GeneratedCode {
    pub cargo_toml: String,
    pub main_rs: String,
    pub readme_md: String,
}

/// Generate Cargo.toml content.
fn generate_cargo_toml(state: &ProjectState, name: &str) -> String {
    let description = state
        .description
        .as_deref()
        .unwrap_or("An MCP server built with tower-mcp");

    let mut features = vec![];
    for transport in &state.transports {
        match transport {
            Transport::Http => features.push("http"),
            Transport::WebSocket => features.push("websocket"),
            Transport::Stdio => {} // No feature needed
        }
    }

    let features_str = if features.is_empty() {
        String::new()
    } else {
        format!(
            ", features = [{}]",
            features
                .iter()
                .map(|f| format!("\"{}\"", f))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"
description = "{description}"

[dependencies]
tower-mcp = {{ version = "0.2"{features_str} }}
tokio = {{ version = "1", features = ["full"] }}
serde = {{ version = "1", features = ["derive"] }}
schemars = "1.2"
tracing = "0.1"
tracing-subscriber = {{ version = "0.3", features = ["env-filter"] }}
"#
    )
}

/// Generate README.md content.
fn generate_readme(state: &ProjectState, name: &str) -> String {
    let description = state
        .description
        .as_deref()
        .unwrap_or("An MCP server built with tower-mcp");

    let tool_list = if state.tools.is_empty() {
        "No tools defined yet.".to_string()
    } else {
        state
            .tools
            .iter()
            .map(|t| format!("- **{}** - {}", t.name, t.description))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let transport_info =
        if state.transports.is_empty() || state.transports.contains(&Transport::Stdio) {
            "stdio (run as a child process)"
        } else if state.transports.contains(&Transport::Http) {
            "HTTP (run as a web server)"
        } else {
            "WebSocket"
        };

    format!(
        r#"# {name}

{description}

## Tools

{tool_list}

## Getting Started

1. **Build the server:**
   ```bash
   cargo build --release
   ```

2. **Run the server:**
   ```bash
   cargo run
   ```

   Transport: {transport_info}

3. **Add to your MCP client** (e.g., Claude Code's `.mcp.json`):
   ```json
   {{
     "{name}": {{
       "command": "cargo",
       "args": ["run", "--manifest-path", "path/to/{name}/Cargo.toml"]
     }}
   }}
   ```

## What's Next?

You now have a working MCP server skeleton. You can:

**Option A: Stop here and implement manually**

The generated code compiles and runs. Open `src/main.rs`, find the
`// TODO: implement` comments, and add your logic. You have everything
you need.

**Option B: Keep using codegen-mcp**

Continue adding tools, resources, or prompts interactively. The server
will regenerate as you make changes.

## Implementing Handlers

The generated handlers return debug output. To add real functionality:

1. Open `src/main.rs`
2. Find the `// TODO: implement` comments
3. Replace the placeholder with your logic

### Common patterns:

**HTTP requests** - Add `reqwest` to Cargo.toml:
```toml
reqwest = {{ version = "0.12", features = ["json"] }}
```

**Error handling:**
```rust
match do_something().await {{
    Ok(result) => Ok(CallToolResult::text(result)),
    Err(e) => Ok(CallToolResult::text(format!("Error: {{}}", e))),
}}
```

**Returning structured data:**
```rust
let data = serde_json::json!({{ "count": 42, "items": ["a", "b"] }});
Ok(CallToolResult::text(data.to_string()))
```

## Learn More

- [tower-mcp documentation](https://docs.rs/tower-mcp)
- [MCP specification](https://modelcontextprotocol.io)
- [Example servers](https://github.com/joshrotenberg/tower-mcp/tree/main/examples)
"#
    )
}

/// Generate main.rs content.
fn generate_main_rs(state: &ProjectState) -> String {
    let mut code = String::new();

    // Imports
    code.push_str(&generate_imports(state));
    code.push('\n');

    // Input structs for tools
    for tool in &state.tools {
        if !tool.input_fields.is_empty()
            && !matches!(tool.handler_type, HandlerType::Raw | HandlerType::NoParams)
        {
            code.push_str(&generate_input_struct(tool));
            code.push('\n');
        }
    }

    // State struct if needed
    if state.has_state() {
        code.push_str(&generate_state_struct(state));
        code.push('\n');
    }

    // Main function
    code.push_str(&generate_main_function(state));

    code
}

/// Generate import statements.
fn generate_imports(state: &ProjectState) -> String {
    let mut imports = vec![
        "use schemars::JsonSchema;",
        "use serde::Deserialize;",
        "use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};",
    ];

    // Add transport imports
    let has_stdio = state.transports.contains(&Transport::Stdio) || state.transports.is_empty();
    let has_http = state.transports.contains(&Transport::Http);
    let has_ws = state.transports.contains(&Transport::WebSocket);

    if has_stdio {
        imports.push("use tower_mcp::StdioTransport;");
    }
    if has_http {
        imports.push("use tower_mcp::HttpTransport;");
    }
    if has_ws {
        imports.push("use tower_mcp::WebSocketTransport;");
    }

    // Add state imports if needed
    if state.any_tool_uses_state() || state.has_state() {
        imports.push("use std::sync::Arc;");
    }

    // Add context import if needed
    if state.any_tool_uses_context() {
        imports.push("use tower_mcp::RequestContext;");
    }

    imports.join("\n")
}

/// Generate an input struct for a tool.
fn generate_input_struct(tool: &ToolDef) -> String {
    let struct_name = to_pascal_case(&tool.name) + "Input";

    let mut code = format!(
        "#[allow(dead_code)]\n#[derive(Debug, Deserialize, JsonSchema)]\nstruct {} {{\n",
        struct_name
    );

    for field in &tool.input_fields {
        // Doc comment
        code.push_str(&format!("    /// {}\n", field.description));

        // Serde attributes for optional fields
        if !field.required {
            code.push_str("    #[serde(default)]\n");
        }

        // Field definition
        let rust_type = to_rust_type(&field.field_type, field.required);
        code.push_str(&format!("    {}: {},\n", field.name, rust_type));
    }

    code.push_str("}\n");
    code
}

/// Generate the state struct.
fn generate_state_struct(state: &ProjectState) -> String {
    let mut code = String::from("/// Shared application state.\nstruct AppState {\n");

    for field in &state.state_fields {
        if let Some(desc) = &field.description {
            code.push_str(&format!("    /// {}\n", desc));
        }
        code.push_str(&format!("    {}: {},\n", field.name, field.field_type));
    }

    code.push_str("}\n");
    code
}

/// Generate the main function.
fn generate_main_function(state: &ProjectState) -> String {
    let name = state.name.as_deref().unwrap_or("mcp-server");
    let description = state.description.as_deref().unwrap_or("An MCP server");

    let mut code = String::from(
        r#"#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .init();

"#,
    );

    // Create state if needed
    if state.has_state() {
        code.push_str("    let state = Arc::new(AppState {\n");
        for field in &state.state_fields {
            code.push_str(&format!(
                "        {}: todo!(\"initialize {}\"),\n",
                field.name, field.name
            ));
        }
        code.push_str("    });\n\n");
    }

    // Build tools
    for tool in &state.tools {
        code.push_str(&generate_tool_builder(tool, state.has_state()));
        code.push('\n');
    }

    // Create router
    code.push_str(&format!(
        r#"    let router = McpRouter::new()
        .server_info("{}", "0.1.0")
        .instructions("{}")
"#,
        name, description
    ));

    for tool in &state.tools {
        code.push_str(&format!("        .tool({})\n", tool.name));
    }

    // Remove trailing newline and add semicolon
    if code.ends_with('\n') {
        code.pop();
    }
    code.push_str(";\n\n");

    // Transport setup
    let primary_transport = state.transports.first().unwrap_or(&Transport::Stdio);
    match primary_transport {
        Transport::Stdio => {
            code.push_str(
                r#"    let mut transport = StdioTransport::new(router);
    transport.run().await?;
"#,
            );
        }
        Transport::Http => {
            code.push_str(
                r#"    let transport = HttpTransport::new(router);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::info!("Starting HTTP server on {}", addr);
    transport.serve(addr).await?;
"#,
            );
        }
        Transport::WebSocket => {
            code.push_str(
                r#"    let transport = WebSocketTransport::new(router);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::info!("Starting WebSocket server on {}", addr);
    transport.serve(addr).await?;
"#,
            );
        }
    }

    code.push_str("\n    Ok(())\n}\n");
    code
}

/// Generate a tool builder.
fn generate_tool_builder(tool: &ToolDef, _has_state: bool) -> String {
    let mut code = format!(
        "    let {} = ToolBuilder::new(\"{}\")\n        .description(\"{}\")\n",
        tool.name, tool.name, tool.description
    );

    // Annotations
    if tool.annotations.read_only {
        code.push_str("        .read_only()\n");
    }
    if tool.annotations.idempotent {
        code.push_str("        .idempotent()\n");
    }

    // Handler
    let input_type = if tool.input_fields.is_empty()
        || matches!(tool.handler_type, HandlerType::Raw | HandlerType::NoParams)
    {
        None
    } else {
        Some(to_pascal_case(&tool.name) + "Input")
    };

    match tool.handler_type {
        HandlerType::Simple => {
            if let Some(ref input) = input_type {
                let example = generate_example_impl(tool, false, false);
                code.push_str(&format!(
                    r#"        .handler(|input: {}| async move {{
            // TODO: implement {}
{}
            Ok(CallToolResult::text(format!("{{:?}}", input)))
        }})
"#,
                    input, tool.name, example
                ));
            } else {
                code.push_str(&format!(
                    r#"        .handler_no_params(|| async move {{
            // TODO: implement {}
            // Example: Ok(CallToolResult::text("Done!"))
            Ok(CallToolResult::text("OK"))
        }})
"#,
                    tool.name
                ));
            }
        }
        HandlerType::WithState => {
            if let Some(ref input) = input_type {
                let example = generate_example_impl(tool, true, false);
                code.push_str(&format!(
                    r#"        .handler_with_state(state.clone(), |state: Arc<AppState>, input: {}| async move {{
            // TODO: implement {}
{}
            Ok(CallToolResult::text(format!("{{:?}}", input)))
        }})
"#,
                    input, tool.name, example
                ));
            } else {
                code.push_str(&format!(
                    r#"        .handler_with_state_no_params(state.clone(), |state: Arc<AppState>| async move {{
            // TODO: implement {}
            // Example: let data = state.db.query(...).await?;
            Ok(CallToolResult::text("OK"))
        }})
"#,
                    tool.name
                ));
            }
        }
        HandlerType::WithContext => {
            if let Some(ref input) = input_type {
                let example = generate_example_impl(tool, false, true);
                code.push_str(&format!(
                    r#"        .handler_with_context(|ctx: RequestContext, input: {}| async move {{
            // TODO: implement {}
{}
            Ok(CallToolResult::text(format!("{{:?}}", input)))
        }})
"#,
                    input, tool.name, example
                ));
            } else {
                code.push_str(&format!(
                    r#"        .handler_no_params_with_context(|ctx: RequestContext| async move {{
            // TODO: implement {}
            // Example: ctx.send_progress(0.5, Some(50), Some("Halfway done")).await;
            Ok(CallToolResult::text("OK"))
        }})
"#,
                    tool.name
                ));
            }
        }
        HandlerType::WithStateAndContext => {
            if let Some(ref input) = input_type {
                let example = generate_example_impl(tool, true, true);
                code.push_str(&format!(
                    r#"        .handler_with_state_and_context(state.clone(), |state: Arc<AppState>, ctx: RequestContext, input: {}| async move {{
            // TODO: implement {}
{}
            Ok(CallToolResult::text(format!("{{:?}}", input)))
        }})
"#,
                    input, tool.name, example
                ));
            } else {
                code.push_str(&format!(
                    r#"        .handler_with_state_and_context_no_params(state.clone(), |state: Arc<AppState>, ctx: RequestContext| async move {{
            // TODO: implement {}
            // Example:
            //   ctx.send_progress(0.0, Some(0), Some("Starting")).await;
            //   let result = state.client.fetch(...).await?;
            //   ctx.send_progress(1.0, Some(100), Some("Done")).await;
            Ok(CallToolResult::text("OK"))
        }})
"#,
                    tool.name
                ));
            }
        }
        HandlerType::Raw => {
            code.push_str(&format!(
                r#"        .raw_handler(|args: serde_json::Value| async move {{
            // TODO: implement {}
            // Example:
            //   let id = args.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            //   Ok(CallToolResult::text(format!("Got id: {{}}", id)))
            Ok(CallToolResult::text(format!("{{}}", args)))
        }})
"#,
                tool.name
            ));
        }
        HandlerType::NoParams => {
            code.push_str(&format!(
                r#"        .handler_no_params(|| async move {{
            // TODO: implement {}
            // Example: Ok(CallToolResult::text("Done!"))
            Ok(CallToolResult::text("OK"))
        }})
"#,
                tool.name
            ));
        }
    }

    code.push_str("        .build()?;\n");
    code
}

/// Generate example implementation comments based on input fields.
fn generate_example_impl(tool: &ToolDef, has_state: bool, has_context: bool) -> String {
    let mut lines = vec!["            // Example:".to_string()];

    // Add context example if available
    if has_context {
        lines.push(
            "            //   ctx.send_progress(0.5, Some(50), Some(\"Processing\")).await;"
                .to_string(),
        );
    }

    // Generate field usage examples
    if !tool.input_fields.is_empty() {
        let field = &tool.input_fields[0];
        let field_example = match field.field_type.to_lowercase().as_str() {
            "string" => format!(
                "            //   let result = format!(\"Got: {{}}\", input.{});",
                field.name
            ),
            "i64" | "int" | "integer" => {
                format!("            //   let doubled = input.{} * 2;", field.name)
            }
            "bool" | "boolean" => format!(
                "            //   if input.{} {{ /* do something */ }}",
                field.name
            ),
            _ => format!("            //   let value = &input.{};", field.name),
        };
        lines.push(field_example);
    }

    // Add state example if available
    if has_state {
        lines.push("            //   let data = state.client.fetch(...).await?;".to_string());
    }

    // Add return example
    lines.push("            //   Ok(CallToolResult::text(result))".to_string());

    lines.join("\n")
}

/// Convert snake_case to PascalCase.
fn to_pascal_case(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().chain(chars).collect(),
            }
        })
        .collect()
}

/// Convert a type string to Rust type.
fn to_rust_type(type_str: &str, required: bool) -> String {
    let base_type = match type_str.to_lowercase().as_str() {
        "string" => "String",
        "i64" | "int" | "integer" => "i64",
        "f64" | "float" | "number" => "f64",
        "bool" | "boolean" => "bool",
        _ if type_str.starts_with("Option<") => return type_str.to_string(),
        _ if type_str.starts_with("Vec<") => return type_str.to_string(),
        _ => type_str, // Pass through custom types
    };

    if required {
        base_type.to_string()
    } else {
        format!("Option<{}>", base_type)
    }
}
