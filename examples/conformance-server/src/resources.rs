use std::collections::HashMap;
use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
use tower_mcp::{Resource, ResourceBuilder, ResourceTemplate, ResourceTemplateBuilder};

use crate::tools::red_pixel_base64;

/// Build all conformance resources.
pub fn build_resources() -> Vec<Resource> {
    vec![
        build_static_text(),
        build_static_binary(),
        build_embedded_resource(),
        build_watched_resource(),
    ]
}

/// Build all conformance resource templates.
pub fn build_resource_templates() -> Vec<ResourceTemplate> {
    vec![build_template()]
}

fn build_static_text() -> Resource {
    ResourceBuilder::new("test://static-text")
        .name("Static Text Resource")
        .description("A static text resource for conformance testing")
        .mime_type("text/plain")
        .text("This is static text content")
}

fn build_static_binary() -> Resource {
    let png_base64 = red_pixel_base64();
    ResourceBuilder::new("test://static-binary")
        .name("Static Binary Resource")
        .description("A static binary resource (1x1 PNG) for conformance testing")
        .mime_type("image/png")
        .handler(move || {
            let png_base64 = png_base64.clone();
            async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: "test://static-binary".to_string(),
                        mime_type: Some("image/png".to_string()),
                        text: None,
                        blob: Some(png_base64),
                    }],
                })
            }
        })
        .build()
}

fn build_embedded_resource() -> Resource {
    ResourceBuilder::new("test://embedded-resource")
        .name("Embedded Resource")
        .description("A text resource referenced by the embedded resource tool")
        .mime_type("text/plain")
        .text("Embedded resource content")
}

fn build_watched_resource() -> Resource {
    ResourceBuilder::new("test://watched-resource")
        .name("Watched Resource")
        .description("A resource that can be subscribed to for updates")
        .mime_type("text/plain")
        .text("Watched resource content")
}

fn build_template() -> ResourceTemplate {
    ResourceTemplateBuilder::new("test://template/{id}/data")
        .name("Template Resource")
        .description("A parameterized resource template for conformance testing")
        .mime_type("text/plain")
        .handler(|uri: String, vars: HashMap<String, String>| async move {
            let id = vars.get("id").cloned().unwrap_or_default();
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: Some("text/plain".to_string()),
                    text: Some(format!("Template data for id: {}", id)),
                    blob: None,
                }],
            })
        })
}
