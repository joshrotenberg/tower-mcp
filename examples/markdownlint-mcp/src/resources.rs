//! MCP resources for browsing lint rules.
//!
//! This module provides resources for discovering and exploring available lint rules.
//! Resources complement tools by providing read-only data that can be browsed or
//! referenced by clients.
//!
//! # Resource Types
//!
//! ## Static Resources
//!
//! Static resources have fixed URIs and return data that doesn't depend on parameters:
//!
//! - `rules://overview` - JSON overview of all available rules
//!
//! ## Resource Templates
//!
//! Resource templates use URI templates with parameters for dynamic content:
//!
//! - `rules://{rule_id}` - Detailed information about a specific rule
//!
//! # Example Usage
//!
//! ```rust,ignore
//! // In main.rs, register resources with the router
//! let router = McpRouter::new("markdownlint")
//!     .resources(build_resources())
//!     .resource_templates(build_resource_templates());
//! ```

use std::collections::HashMap;
use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
use tower_mcp::{Resource, ResourceBuilder, ResourceTemplate, ResourceTemplateBuilder};

use crate::engine::create_engine;

/// Build static resources.
///
/// Returns a list of static resources to register with the MCP router.
/// Currently includes:
/// - `rules://overview` - Overview of all available lint rules
pub fn build_resources() -> Vec<Resource> {
    vec![build_rules_overview_resource()]
}

/// Build resource templates.
///
/// Returns a list of resource templates to register with the MCP router.
/// Currently includes:
/// - `rules://{rule_id}` - Details for a specific rule
pub fn build_resource_templates() -> Vec<ResourceTemplate> {
    vec![build_rule_detail_template()]
}

fn build_rules_overview_resource() -> Resource {
    ResourceBuilder::new("rules://overview")
        .name("Lint Rules Overview")
        .description("Overview of all available markdown lint rules")
        .mime_type("application/json")
        .handler(|| async {
            let engine = create_engine()?;

            let rules: Vec<_> = engine
                .registry()
                .rules()
                .iter()
                .map(|rule| {
                    serde_json::json!({
                        "id": rule.id(),
                        "name": rule.name(),
                        "description": rule.description(),
                        "can_fix": rule.can_fix(),
                    })
                })
                .collect();

            let overview = serde_json::json!({
                "total_rules": rules.len(),
                "rules": rules,
            });

            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri: "rules://overview".to_string(),
                    mime_type: Some("application/json".to_string()),
                    text: Some(serde_json::to_string_pretty(&overview).unwrap_or_default()),
                    blob: None,
                }],
            })
        })
        .build()
}

fn build_rule_detail_template() -> ResourceTemplate {
    ResourceTemplateBuilder::new("rules://{rule_id}")
        .name("Lint Rule Details")
        .description("Detailed information about a specific lint rule")
        .mime_type("application/json")
        .handler(|uri: String, vars: HashMap<String, String>| async move {
            let rule_id = vars.get("rule_id").cloned().unwrap_or_default();

            let engine = create_engine()?;

            let rule = engine.registry().get_rule(&rule_id);

            match rule {
                Some(rule) => {
                    let detail = serde_json::json!({
                        "id": rule.id(),
                        "name": rule.name(),
                        "description": rule.description(),
                        "can_fix": rule.can_fix(),
                    });

                    Ok(ReadResourceResult {
                        contents: vec![ResourceContent {
                            uri,
                            mime_type: Some("application/json".to_string()),
                            text: Some(serde_json::to_string_pretty(&detail).unwrap_or_default()),
                            blob: None,
                        }],
                    })
                }
                None => Err(tower_mcp::Error::internal(format!(
                    "Rule '{}' not found",
                    rule_id
                ))),
            }
        })
}
