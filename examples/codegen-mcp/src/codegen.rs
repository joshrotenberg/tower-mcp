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
    })
}

/// The generated code files.
pub struct GeneratedCode {
    pub cargo_toml: String,
    pub main_rs: String,
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
tower-mcp = {{ version = "0.3"{features_str} }}
tokio = {{ version = "1", features = ["full"] }}
serde = {{ version = "1", features = ["derive"] }}
schemars = "1.2"
tracing = "0.1"
tracing-subscriber = {{ version = "0.3", features = ["env-filter"] }}
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
        "#[derive(Debug, Deserialize, JsonSchema)]\nstruct {} {{\n",
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
            if let Some(input) = input_type {
                code.push_str(&format!(
                    "        .handler(|input: {}| async move {{\n            todo!(\"implement {}\")\n        }})\n",
                    input, tool.name
                ));
            } else {
                code.push_str(&format!(
                    "        .handler_no_params(|| async move {{\n            todo!(\"implement {}\")\n        }})\n",
                    tool.name
                ));
            }
        }
        HandlerType::WithState => {
            if let Some(input) = input_type {
                code.push_str(&format!(
                    "        .handler_with_state(state.clone(), |state: Arc<AppState>, input: {}| async move {{\n            todo!(\"implement {}\")\n        }})\n",
                    input, tool.name
                ));
            } else {
                code.push_str(&format!(
                    "        .handler_with_state_no_params(state.clone(), |state: Arc<AppState>| async move {{\n            todo!(\"implement {}\")\n        }})\n",
                    tool.name
                ));
            }
        }
        HandlerType::WithContext => {
            if let Some(input) = input_type {
                code.push_str(&format!(
                    "        .handler_with_context(|ctx: RequestContext, input: {}| async move {{\n            todo!(\"implement {}\")\n        }})\n",
                    input, tool.name
                ));
            } else {
                code.push_str(&format!(
                    "        .handler_no_params_with_context(|ctx: RequestContext| async move {{\n            todo!(\"implement {}\")\n        }})\n",
                    tool.name
                ));
            }
        }
        HandlerType::WithStateAndContext => {
            if let Some(input) = input_type {
                code.push_str(&format!(
                    "        .handler_with_state_and_context(state.clone(), |state: Arc<AppState>, ctx: RequestContext, input: {}| async move {{\n            todo!(\"implement {}\")\n        }})\n",
                    input, tool.name
                ));
            } else {
                code.push_str(&format!(
                    "        .handler_with_state_and_context_no_params(state.clone(), |state: Arc<AppState>, ctx: RequestContext| async move {{\n            todo!(\"implement {}\")\n        }})\n",
                    tool.name
                ));
            }
        }
        HandlerType::Raw => {
            code.push_str(&format!(
                "        .raw_handler(|args: serde_json::Value| async move {{\n            todo!(\"implement {}\")\n        }})\n",
                tool.name
            ));
        }
        HandlerType::NoParams => {
            code.push_str(&format!(
                "        .handler_no_params(|| async move {{\n            todo!(\"implement {}\")\n        }})\n",
                tool.name
            ));
        }
    }

    code.push_str("        .build()?;\n");
    code
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
