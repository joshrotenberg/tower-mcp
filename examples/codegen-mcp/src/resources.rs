//! MCP resource definitions for code generation.
//!
//! Exposes the generated code and project state as browsable resources.

use std::sync::Arc;

use tower_mcp::{ReadResourceResult, Resource, ResourceBuilder, ResourceContent};

use crate::codegen::generate_code;
use crate::tools::CodegenState;

/// Build all codegen resources.
pub fn build_resources(state: Arc<CodegenState>) -> Vec<Resource> {
    vec![
        build_cargo_toml_resource(state.clone()),
        build_main_rs_resource(state.clone()),
        build_readme_resource(state.clone()),
        build_state_resource(state.clone()),
    ]
}

fn build_cargo_toml_resource(state: Arc<CodegenState>) -> Resource {
    ResourceBuilder::new("project://Cargo.toml")
        .name("Generated Cargo.toml")
        .description("The generated Cargo.toml for the current project")
        .mime_type("text/x-toml")
        .handler(move || {
            let state = state.clone();
            async move {
                let project = state.project.read().await;

                let text = if !project.initialized {
                    "# Project not initialized. Call init_project first.".to_string()
                } else {
                    match generate_code(&project) {
                        Ok(code) => code.cargo_toml,
                        Err(e) => format!("# Error generating code: {}", e),
                    }
                };

                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: "project://Cargo.toml".to_string(),
                        mime_type: Some("text/x-toml".to_string()),
                        text: Some(text),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                })
            }
        })
        .build()
}

fn build_main_rs_resource(state: Arc<CodegenState>) -> Resource {
    ResourceBuilder::new("project://src/main.rs")
        .name("Generated main.rs")
        .description("The generated src/main.rs for the current project")
        .mime_type("text/x-rust")
        .handler(move || {
            let state = state.clone();
            async move {
                let project = state.project.read().await;

                let text = if !project.initialized {
                    "// Project not initialized. Call init_project first.".to_string()
                } else {
                    match generate_code(&project) {
                        Ok(code) => code.main_rs,
                        Err(e) => format!("// Error generating code: {}", e),
                    }
                };

                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: "project://src/main.rs".to_string(),
                        mime_type: Some("text/x-rust".to_string()),
                        text: Some(text),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                })
            }
        })
        .build()
}

fn build_readme_resource(state: Arc<CodegenState>) -> Resource {
    ResourceBuilder::new("project://README.md")
        .name("Generated README.md")
        .description("A README with getting started instructions")
        .mime_type("text/markdown")
        .handler(move || {
            let state = state.clone();
            async move {
                let project = state.project.read().await;

                let text = if !project.initialized {
                    "# Project not initialized\n\nCall `init_project` first.".to_string()
                } else {
                    match generate_code(&project) {
                        Ok(code) => code.readme_md,
                        Err(e) => format!("# Error\n\n{}", e),
                    }
                };

                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: "project://README.md".to_string(),
                        mime_type: Some("text/markdown".to_string()),
                        text: Some(text),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                })
            }
        })
        .build()
}

fn build_state_resource(state: Arc<CodegenState>) -> Resource {
    ResourceBuilder::new("project://state.json")
        .name("Project State")
        .description("The current project state as JSON")
        .mime_type("application/json")
        .handler(move || {
            let state = state.clone();
            async move {
                let project = state.project.read().await;

                let json = serde_json::to_string_pretty(&*project)
                    .unwrap_or_else(|e| format!("{{\"error\": \"{}\"}}", e));

                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: "project://state.json".to_string(),
                        mime_type: Some("application/json".to_string()),
                        text: Some(json),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                })
            }
        })
        .build()
}
