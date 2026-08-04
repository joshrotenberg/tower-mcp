//! Deterministic Markdown rendering and documentation assessment for a
//! discovered MCP surface.
//!
//! The [`Snapshot`] type is lifecycle-neutral: a caller can collect it through
//! stable `initialize` or final `server/discover`, then use the same rendering
//! and assessment functions. The library itself performs no network requests
//! and invokes no server operations. The `mcp2md` binary is the reference
//! collector; it only performs the handshake and advertised list operations.
//!
//! Documentation coverage measures whether descriptions are present for the
//! server, surface entries, named schema fields, and prompt or resource-template
//! arguments. It does not claim those descriptions are accurate or useful.
//! Optional presentation and contract metadata is reported separately so a
//! missing title or output schema cannot distort the documentation score.
//!
//! # Example
//!
//! ```rust
//! use mcp2md::{RenderOptions, Snapshot, assess, render_markdown};
//! use tower_mcp::protocol::{Implementation, ServerCapabilities};
//!
//! let mut snapshot = Snapshot {
//!     protocol_version: "2025-11-25".into(),
//!     supported_versions: None,
//!     server_info: Implementation {
//!         name: "weather".into(),
//!         version: "1.0.0".into(),
//!         description: Some("Weather forecasts and alerts.".into()),
//!         ..Default::default()
//!     },
//!     capabilities: ServerCapabilities::default(),
//!     instructions: None,
//!     tools: Vec::new(),
//!     prompts: Vec::new(),
//!     resources: Vec::new(),
//!     resource_templates: Vec::new(),
//! };
//! snapshot.sort();
//!
//! let assessment = assess(&snapshot);
//! assert_eq!(assessment.overall.percentage(), 100);
//! let markdown = render_markdown(&snapshot, RenderOptions::default());
//! assert!(markdown.contains("# weather MCP server"));
//! ```

#![deny(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::Serialize;
use serde_json::Value;
use tower_mcp::protocol::{
    Implementation, PromptDefinition, ResourceDefinition, ResourceTemplateDefinition,
    ServerCapabilities, ToolDefinition,
};

/// A lifecycle-neutral snapshot of everything mcp2md learns without invoking
/// a server operation that can have application side effects.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// Protocol version selected for the inspected connection.
    pub protocol_version: String,
    /// Exhaustive versions reported by `server/discover`. Legacy initialize
    /// only reports the selected version, so this is absent on that lifecycle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supported_versions: Option<Vec<String>>,
    /// Identity and optional presentation metadata advertised by the server.
    pub server_info: Implementation,
    /// Capability shape advertised by the server.
    pub capabilities: ServerCapabilities,
    /// Server-wide usage guidance from initialization or discovery.
    pub instructions: Option<String>,
    /// Complete tool definitions returned by the paginated tools surface.
    pub tools: Vec<ToolDefinition>,
    /// Complete prompt definitions returned by the paginated prompts surface.
    pub prompts: Vec<PromptDefinition>,
    /// Complete concrete resource definitions returned by the paginated
    /// resources surface.
    pub resources: Vec<ResourceDefinition>,
    /// Complete resource-template definitions returned by the paginated
    /// resource-template surface.
    pub resource_templates: Vec<ResourceTemplateDefinition>,
}

impl Snapshot {
    /// Normalize list ordering so identical surfaces produce identical output.
    pub fn sort(&mut self) {
        if let Some(versions) = &mut self.supported_versions {
            versions.sort();
            versions.dedup();
        }
        self.tools.sort_by(|left, right| left.name.cmp(&right.name));
        self.prompts
            .sort_by(|left, right| left.name.cmp(&right.name));
        self.resources
            .sort_by(|left, right| left.uri.cmp(&right.uri));
        self.resource_templates
            .sort_by(|left, right| left.uri_template.cmp(&right.uri_template));
    }
}

/// Controls optional, potentially verbose sections in the generated document.
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// Include the human-readable documentation score and gap list.
    pub assessment: bool,
    /// Include exact schemas and a canonical JSON inventory after the readable
    /// reference.
    pub raw_json: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            assessment: true,
            raw_json: true,
        }
    }
}

/// Count of documented items relative to all applicable items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Coverage {
    /// Applicable items with a non-empty description.
    pub documented: usize,
    /// All applicable items in this scope.
    pub total: usize,
}

impl Coverage {
    /// Rounded whole-number percentage, treating an empty scope as fully
    /// covered because it has no missing documentation.
    #[must_use]
    pub fn percentage(&self) -> usize {
        (self.documented * 100 + self.total / 2)
            .checked_div(self.total)
            .unwrap_or(100)
    }
}

/// Documentation coverage for one named MCP surface area.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverageCategory {
    /// Stable human-readable category name.
    pub name: &'static str,
    /// Coverage counts for the category.
    pub coverage: Coverage,
}

/// One concrete omission that lowers the documentation score.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentationGap {
    /// Stable dotted path to the undocumented server item or field.
    pub path: String,
    /// Suggested documentation improvement.
    pub message: String,
}

/// Optional contract and presentation metadata. These figures are useful,
/// but are deliberately not folded into documentation coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataCoverage {
    /// Surface entries carrying a display title.
    pub titled: Coverage,
    /// Tools carrying an output schema.
    pub tools_with_output_schema: Coverage,
    /// Tools carrying behavior annotations.
    pub tools_with_annotations: Coverage,
    /// Resources and resource templates carrying a MIME type.
    pub resources_with_mime_type: Coverage,
}

/// Complete documentation assessment for one [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentationAssessment {
    /// Aggregate description coverage across every applicable category.
    pub overall: Coverage,
    /// Per-surface description coverage in stable display order.
    pub categories: Vec<CoverageCategory>,
    /// Every concrete omission included in [`Self::overall`].
    pub gaps: Vec<DocumentationGap>,
    /// Informational presentation and contract metadata coverage, excluded
    /// from the documentation score.
    pub metadata: MetadataCoverage,
}

/// Machine-readable assessment with enough server identity to archive or
/// compare reports without also retaining the generated Markdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssessmentReport {
    /// Advertised server implementation name.
    pub server_name: String,
    /// Advertised server implementation version.
    pub server_version: String,
    /// Protocol version selected for the inspected connection.
    pub protocol_version: String,
    /// Convenience copy of the rounded overall percentage.
    pub documentation_score: usize,
    /// Full counts, category breakdown, gaps, and optional metadata coverage.
    pub assessment: DocumentationAssessment,
}

/// Build the structured form written by the CLI's assessment output option.
#[must_use]
pub fn assessment_report(snapshot: &Snapshot) -> AssessmentReport {
    let assessment = assess(snapshot);
    AssessmentReport {
        server_name: snapshot.server_info.name.clone(),
        server_version: snapshot.server_info.version.clone(),
        protocol_version: snapshot.protocol_version.clone(),
        documentation_score: assessment.overall.percentage(),
        assessment,
    }
}

#[derive(Default)]
struct CategoryCounter {
    documented: usize,
    total: usize,
}

/// Evaluate documentation visible through MCP discovery.
///
/// The score covers a server overview, descriptions for surface entries, and
/// descriptions for named tool fields and prompt/template arguments. Optional
/// presentation and behavioral metadata is reported separately.
#[must_use]
pub fn assess(snapshot: &Snapshot) -> DocumentationAssessment {
    let mut categories: BTreeMap<&'static str, CategoryCounter> = [
        ("Server", CategoryCounter::default()),
        ("Tools", CategoryCounter::default()),
        ("Prompts", CategoryCounter::default()),
        ("Resources", CategoryCounter::default()),
        ("Resource templates", CategoryCounter::default()),
    ]
    .into_iter()
    .collect();
    let mut gaps = Vec::new();

    let mut record = |category: &'static str, documented: bool, path: String, message: String| {
        let counter = categories
            .get_mut(category)
            .expect("assessment category is registered");
        counter.total += 1;
        if documented {
            counter.documented += 1;
        } else {
            gaps.push(DocumentationGap { path, message });
        }
    };

    record(
        "Server",
        nonempty(snapshot.server_info.description.as_deref())
            || nonempty(snapshot.instructions.as_deref()),
        "server".to_string(),
        "Add an implementation description or server instructions.".to_string(),
    );

    for tool in &snapshot.tools {
        record(
            "Tools",
            nonempty(tool.description.as_deref()),
            format!("tools.{}", tool.name),
            "Describe what the tool does and when to use it.".to_string(),
        );
        record_schema_fields(
            &mut record,
            "Tools",
            &format!("tools.{}.input", tool.name),
            "input",
            &tool.input_schema,
        );
        if let Some(schema) = &tool.output_schema {
            record_schema_fields(
                &mut record,
                "Tools",
                &format!("tools.{}.output", tool.name),
                "output",
                schema,
            );
        }
    }

    for prompt in &snapshot.prompts {
        record(
            "Prompts",
            nonempty(prompt.description.as_deref()),
            format!("prompts.{}", prompt.name),
            "Describe the prompt's purpose and expected result.".to_string(),
        );
        for argument in &prompt.arguments {
            record(
                "Prompts",
                nonempty(argument.description.as_deref()),
                format!("prompts.{}.arguments.{}", prompt.name, argument.name),
                "Describe this prompt argument.".to_string(),
            );
        }
    }

    for resource in &snapshot.resources {
        record(
            "Resources",
            nonempty(resource.description.as_deref()),
            format!("resources.{}", resource.uri),
            "Describe the resource's contents and intended use.".to_string(),
        );
    }

    for template in &snapshot.resource_templates {
        record(
            "Resource templates",
            nonempty(template.description.as_deref()),
            format!("resourceTemplates.{}", template.uri_template),
            "Describe the resources produced by this template.".to_string(),
        );
        for argument in &template.arguments {
            record(
                "Resource templates",
                nonempty(argument.description.as_deref()),
                format!(
                    "resourceTemplates.{}.arguments.{}",
                    template.uri_template, argument.name
                ),
                "Describe this URI-template argument.".to_string(),
            );
        }
    }

    let category_order = [
        "Server",
        "Tools",
        "Prompts",
        "Resources",
        "Resource templates",
    ];
    let categories: Vec<_> = category_order
        .into_iter()
        .map(|name| {
            let counter = categories.remove(name).expect("category exists");
            CoverageCategory {
                name,
                coverage: Coverage {
                    documented: counter.documented,
                    total: counter.total,
                },
            }
        })
        .collect();
    let overall = Coverage {
        documented: categories
            .iter()
            .map(|category| category.coverage.documented)
            .sum(),
        total: categories
            .iter()
            .map(|category| category.coverage.total)
            .sum(),
    };

    let entries = snapshot.tools.len()
        + snapshot.prompts.len()
        + snapshot.resources.len()
        + snapshot.resource_templates.len();
    let titled = snapshot
        .tools
        .iter()
        .filter(|entry| nonempty(entry.title.as_deref()))
        .count()
        + snapshot
            .prompts
            .iter()
            .filter(|entry| nonempty(entry.title.as_deref()))
            .count()
        + snapshot
            .resources
            .iter()
            .filter(|entry| nonempty(entry.title.as_deref()))
            .count()
        + snapshot
            .resource_templates
            .iter()
            .filter(|entry| nonempty(entry.title.as_deref()))
            .count();
    let resource_entries = snapshot.resources.len() + snapshot.resource_templates.len();
    let resources_with_mime_type = snapshot
        .resources
        .iter()
        .filter(|entry| nonempty(entry.mime_type.as_deref()))
        .count()
        + snapshot
            .resource_templates
            .iter()
            .filter(|entry| nonempty(entry.mime_type.as_deref()))
            .count();

    DocumentationAssessment {
        overall,
        categories,
        gaps,
        metadata: MetadataCoverage {
            titled: Coverage {
                documented: titled,
                total: entries,
            },
            tools_with_output_schema: Coverage {
                documented: snapshot
                    .tools
                    .iter()
                    .filter(|tool| tool.output_schema.is_some())
                    .count(),
                total: snapshot.tools.len(),
            },
            tools_with_annotations: Coverage {
                documented: snapshot
                    .tools
                    .iter()
                    .filter(|tool| tool.annotations.is_some())
                    .count(),
                total: snapshot.tools.len(),
            },
            resources_with_mime_type: Coverage {
                documented: resources_with_mime_type,
                total: resource_entries,
            },
        },
    }
}

fn record_schema_fields(
    record: &mut impl FnMut(&'static str, bool, String, String),
    category: &'static str,
    path: &str,
    direction: &str,
    schema: &Value,
) {
    for field in schema_fields(schema) {
        record(
            category,
            nonempty(schema_description(schema, field.schema)),
            format!("{path}.{}", field.path),
            format!("Describe this tool {direction} field."),
        );
    }
}

/// Render one complete Markdown reference.
///
/// Call [`Snapshot::sort`] before rendering when the snapshot was not already
/// collected in deterministic order. All server-controlled text is escaped for
/// its Markdown context, but the returned document is still untrusted content
/// and should pass through the publishing system's normal review process.
#[must_use]
pub fn render_markdown(snapshot: &Snapshot, options: RenderOptions) -> String {
    let mut output = String::new();
    let display_name = snapshot
        .server_info
        .title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&snapshot.server_info.name);

    writeln!(output, "# {} MCP server\n", heading_text(display_name)).unwrap();
    writeln!(
        output,
        "> Generated from MCP discovery. No tools or prompts were invoked, and no resources were read.\n"
    )
    .unwrap();

    render_server(snapshot, &mut output);
    render_surface_summary(snapshot, &mut output);
    if options.assessment {
        render_assessment(&assess(snapshot), &mut output);
    }
    render_tools(snapshot, options.raw_json, &mut output);
    render_prompts(snapshot, &mut output);
    render_resources(snapshot, &mut output);
    render_resource_templates(snapshot, &mut output);
    if options.raw_json {
        writeln!(output, "## Raw protocol inventory\n").unwrap();
        writeln!(
            output,
            "The following canonical JSON preserves metadata not expanded in the readable reference.\n"
        )
        .unwrap();
        let value = serde_json::to_value(snapshot).expect("snapshot is serializable");
        write_fenced_json(&mut output, &value);
    }
    output
}

fn render_server(snapshot: &Snapshot, output: &mut String) {
    writeln!(output, "## Server\n").unwrap();
    writeln!(output, "| Field | Value |").unwrap();
    writeln!(output, "| --- | --- |").unwrap();
    table_row(output, "Name", &inline_code(&snapshot.server_info.name));
    table_row(
        output,
        "Version",
        &inline_code(&snapshot.server_info.version),
    );
    table_row(output, "Protocol", &inline_code(&snapshot.protocol_version));
    if let Some(versions) = &snapshot.supported_versions {
        table_row(
            output,
            "Supported versions",
            &versions
                .iter()
                .map(|version| inline_code(version))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if let Some(title) = nonempty_value(snapshot.server_info.title.as_deref()) {
        table_row(output, "Title", &table_cell(title));
    }
    if let Some(url) = nonempty_value(snapshot.server_info.website_url.as_deref()) {
        table_row(output, "Website", &format!("<{}>", escape_html(url)));
    }
    writeln!(output).unwrap();

    if let Some(description) = nonempty_value(snapshot.server_info.description.as_deref()) {
        writeln!(output, "### Description\n").unwrap();
        write_quote(output, description);
    }
    if let Some(instructions) = nonempty_value(snapshot.instructions.as_deref()) {
        writeln!(output, "### Instructions\n").unwrap();
        write_quote(output, instructions);
    }

    writeln!(output, "### Capabilities\n").unwrap();
    let capabilities = canonical_json(
        serde_json::to_value(&snapshot.capabilities).expect("capabilities are serializable"),
    );
    let Some(entries) = capabilities
        .as_object()
        .filter(|entries| !entries.is_empty())
    else {
        writeln!(output, "No capabilities advertised.\n").unwrap();
        return;
    };
    writeln!(output, "| Capability | Settings |").unwrap();
    writeln!(output, "| --- | --- |").unwrap();
    for (name, settings) in entries {
        table_row(
            output,
            &inline_code(name),
            &inline_code(&compact_json(settings)),
        );
    }
    writeln!(output).unwrap();
}

fn render_surface_summary(snapshot: &Snapshot, output: &mut String) {
    writeln!(output, "## Surface summary\n").unwrap();
    writeln!(output, "| Kind | Count |").unwrap();
    writeln!(output, "| --- | ---: |").unwrap();
    writeln!(output, "| Tools | {} |", snapshot.tools.len()).unwrap();
    writeln!(output, "| Prompts | {} |", snapshot.prompts.len()).unwrap();
    writeln!(output, "| Resources | {} |", snapshot.resources.len()).unwrap();
    writeln!(
        output,
        "| Resource templates | {} |\n",
        snapshot.resource_templates.len()
    )
    .unwrap();
}

fn render_assessment(assessment: &DocumentationAssessment, output: &mut String) {
    writeln!(output, "## Documentation assessment\n").unwrap();
    writeln!(
        output,
        "**{}% documented** ({}/{} applicable descriptions present). This measures presence, not accuracy or usefulness.\n",
        assessment.overall.percentage(),
        assessment.overall.documented,
        assessment.overall.total,
    )
    .unwrap();
    writeln!(output, "| Area | Coverage |").unwrap();
    writeln!(output, "| --- | ---: |").unwrap();
    for category in &assessment.categories {
        let value = if category.coverage.total == 0 {
            "Not applicable".to_string()
        } else {
            format!(
                "{}% ({}/{})",
                category.coverage.percentage(),
                category.coverage.documented,
                category.coverage.total
            )
        };
        table_row(output, category.name, &value);
    }
    writeln!(output).unwrap();

    writeln!(output, "### Optional metadata coverage\n").unwrap();
    writeln!(
        output,
        "These fields improve generated references and client UIs but do not affect the documentation score.\n"
    )
    .unwrap();
    writeln!(output, "| Metadata | Present |").unwrap();
    writeln!(output, "| --- | ---: |").unwrap();
    metadata_row(output, "Entry titles", &assessment.metadata.titled);
    metadata_row(
        output,
        "Tool output schemas",
        &assessment.metadata.tools_with_output_schema,
    );
    metadata_row(
        output,
        "Tool behavior annotations",
        &assessment.metadata.tools_with_annotations,
    );
    metadata_row(
        output,
        "Resource MIME types",
        &assessment.metadata.resources_with_mime_type,
    );
    writeln!(output).unwrap();

    writeln!(output, "### Documentation gaps\n").unwrap();
    if assessment.gaps.is_empty() {
        writeln!(output, "No missing descriptions detected.\n").unwrap();
    } else {
        for gap in &assessment.gaps {
            writeln!(
                output,
                "- {} — {}",
                inline_code(&gap.path),
                escape_html(&gap.message)
            )
            .unwrap();
        }
        writeln!(output).unwrap();
    }
}

fn metadata_row(output: &mut String, label: &str, coverage: &Coverage) {
    let value = if coverage.total == 0 {
        "Not applicable".to_string()
    } else {
        format!("{}/{}", coverage.documented, coverage.total)
    };
    table_row(output, label, &value);
}

fn render_tools(snapshot: &Snapshot, raw_json: bool, output: &mut String) {
    writeln!(output, "## Tools\n").unwrap();
    if snapshot.tools.is_empty() {
        writeln!(output, "No tools advertised.\n").unwrap();
        return;
    }
    for tool in &snapshot.tools {
        render_entry_heading(output, &tool.name, tool.title.as_deref());
        write_description(output, tool.description.as_deref());
        if let Some(annotations) = &tool.annotations {
            writeln!(output, "**Behavior hints**\n").unwrap();
            writeln!(output, "| Hint | Value |").unwrap();
            writeln!(output, "| --- | --- |").unwrap();
            table_row(output, "Read only", &annotations.read_only_hint.to_string());
            if annotations.read_only_hint {
                table_row(
                    output,
                    "Potentially destructive",
                    "Not applicable (read-only)",
                );
                table_row(output, "Idempotent", "Not applicable (read-only)");
            } else {
                table_row(
                    output,
                    "Potentially destructive",
                    &annotations.destructive_hint.to_string(),
                );
                table_row(
                    output,
                    "Idempotent",
                    &annotations.idempotent_hint.to_string(),
                );
            }
            table_row(
                output,
                "Open world",
                &annotations.open_world_hint.to_string(),
            );
            writeln!(output).unwrap();
        } else {
            writeln!(
                output,
                "**Behavior hints:** Not advertised; protocol defaults apply.\n"
            )
            .unwrap();
        }

        render_schema_summary(output, "Parameters", &tool.input_schema);
        if raw_json {
            writeln!(output, "**Input schema**\n").unwrap();
            write_fenced_json(output, &tool.input_schema);
        }
        if let Some(schema) = &tool.output_schema {
            render_schema_summary(output, "Output", schema);
            if raw_json {
                writeln!(output, "**Output schema**\n").unwrap();
                write_fenced_json(output, schema);
            }
        } else {
            writeln!(output, "**Output schema:** Not advertised.\n").unwrap();
        }
    }
}

fn render_prompts(snapshot: &Snapshot, output: &mut String) {
    writeln!(output, "## Prompts\n").unwrap();
    if snapshot.prompts.is_empty() {
        writeln!(output, "No prompts advertised.\n").unwrap();
        return;
    }
    for prompt in &snapshot.prompts {
        render_entry_heading(output, &prompt.name, prompt.title.as_deref());
        write_description(output, prompt.description.as_deref());
        render_arguments(output, &prompt.arguments);
    }
}

fn render_resources(snapshot: &Snapshot, output: &mut String) {
    writeln!(output, "## Resources\n").unwrap();
    if snapshot.resources.is_empty() {
        writeln!(output, "No resources advertised.\n").unwrap();
        return;
    }
    for resource in &snapshot.resources {
        render_entry_heading(output, &resource.name, resource.title.as_deref());
        write_description(output, resource.description.as_deref());
        writeln!(output, "| Field | Value |").unwrap();
        writeln!(output, "| --- | --- |").unwrap();
        table_row(output, "URI", &inline_code(&resource.uri));
        table_row(
            output,
            "MIME type",
            &resource
                .mime_type
                .as_deref()
                .map(inline_code)
                .unwrap_or_else(|| "Not advertised".to_string()),
        );
        if let Some(size) = resource.size {
            table_row(output, "Size", &format!("{size} bytes"));
        }
        writeln!(output).unwrap();
    }
}

fn render_resource_templates(snapshot: &Snapshot, output: &mut String) {
    writeln!(output, "## Resource templates\n").unwrap();
    if snapshot.resource_templates.is_empty() {
        writeln!(output, "No resource templates advertised.\n").unwrap();
        return;
    }
    for template in &snapshot.resource_templates {
        render_entry_heading(output, &template.name, template.title.as_deref());
        write_description(output, template.description.as_deref());
        writeln!(output, "| Field | Value |").unwrap();
        writeln!(output, "| --- | --- |").unwrap();
        table_row(output, "URI template", &inline_code(&template.uri_template));
        table_row(
            output,
            "MIME type",
            &template
                .mime_type
                .as_deref()
                .map(inline_code)
                .unwrap_or_else(|| "Not advertised".to_string()),
        );
        writeln!(output).unwrap();
        render_arguments(output, &template.arguments);
    }
}

fn render_arguments(output: &mut String, arguments: &[tower_mcp::protocol::PromptArgument]) {
    if arguments.is_empty() {
        writeln!(output, "**Arguments:** None.\n").unwrap();
        return;
    }
    writeln!(output, "| Argument | Required | Description |").unwrap();
    writeln!(output, "| --- | :---: | --- |").unwrap();
    for argument in arguments {
        writeln!(
            output,
            "| {} | {} | {} |",
            inline_code(&argument.name),
            if argument.required { "Yes" } else { "No" },
            argument
                .description
                .as_deref()
                .map(table_cell)
                .unwrap_or_else(|| "—".to_string())
        )
        .unwrap();
    }
    writeln!(output).unwrap();
}

fn render_schema_summary(output: &mut String, label: &str, schema: &Value) {
    let fields = schema_fields(schema);
    if fields.is_empty() {
        let schema_type = schema_type(schema, schema);
        writeln!(
            output,
            "**{label}:** No named fields (schema type: {}).\n",
            inline_code(&schema_type)
        )
        .unwrap();
        return;
    }
    writeln!(output, "**{label}**\n").unwrap();
    writeln!(
        output,
        "| Field | Type | Required | Description | Default |"
    )
    .unwrap();
    writeln!(output, "| --- | --- | :---: | --- | --- |").unwrap();
    for field in fields {
        let resolved = resolve_local_ref(schema, field.schema);
        let default = resolved
            .get("default")
            .map(|value| inline_code(&compact_json(value)))
            .unwrap_or_else(|| "—".to_string());
        writeln!(
            output,
            "| {} | {} | {} | {} | {} |",
            inline_code(&field.path),
            inline_code(&schema_type(schema, field.schema)),
            if field.required { "Yes" } else { "No" },
            schema_description(schema, field.schema)
                .map(table_cell)
                .unwrap_or_else(|| "—".to_string()),
            default,
        )
        .unwrap();
    }
    writeln!(output).unwrap();
}

fn render_entry_heading(output: &mut String, name: &str, title: Option<&str>) {
    match nonempty_value(title) {
        Some(title) if title != name => {
            writeln!(
                output,
                "### {} — {}\n",
                inline_code(name),
                heading_text(title)
            )
            .unwrap();
        }
        _ => writeln!(output, "### {}\n", inline_code(name)).unwrap(),
    }
}

fn write_description(output: &mut String, description: Option<&str>) {
    match nonempty_value(description) {
        Some(description) => write_quote(output, description),
        None => writeln!(output, "_No description advertised._\n").unwrap(),
    }
}

fn write_quote(output: &mut String, value: &str) {
    for line in value.lines() {
        writeln!(output, "> {}", escape_html(line)).unwrap();
    }
    writeln!(output).unwrap();
}

fn write_fenced_json(output: &mut String, value: &Value) {
    let json = serde_json::to_string_pretty(&canonical_json(value.clone()))
        .expect("JSON value is serializable");
    let fence = "`".repeat((longest_backtick_run(&json) + 1).max(3));
    writeln!(output, "{fence}json").unwrap();
    writeln!(output, "{json}").unwrap();
    writeln!(output, "{fence}\n").unwrap();
}

fn schema_properties(schema: &Value) -> Vec<(&str, &Value)> {
    let mut properties: Vec<_> = schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|properties| {
            properties
                .iter()
                .map(|(name, value)| (name.as_str(), value))
        })
        .collect();
    properties.sort_by(|left, right| left.0.cmp(right.0));
    properties
}

struct SchemaField<'a> {
    path: String,
    schema: &'a Value,
    required: bool,
}

/// Flatten named object fields into dotted paths. This follows local refs and
/// nested array items while bounding recursive schemas.
fn schema_fields(schema: &Value) -> Vec<SchemaField<'_>> {
    let mut fields = Vec::new();
    let mut refs = BTreeSet::new();
    collect_schema_fields(schema, schema, "", &mut refs, 0, &mut fields);
    fields
}

fn collect_schema_fields<'a>(
    root: &'a Value,
    schema: &'a Value,
    prefix: &str,
    refs: &mut BTreeSet<String>,
    depth: usize,
    fields: &mut Vec<SchemaField<'a>>,
) {
    const MAX_DEPTH: usize = 16;
    if depth >= MAX_DEPTH {
        return;
    }

    let reference = schema
        .get("$ref")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(reference) = &reference
        && !refs.insert(reference.clone())
    {
        return;
    }
    let resolved = resolve_local_ref(root, schema);
    let required = resolved
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();

    for (name, property) in schema_properties(resolved) {
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}.{name}")
        };
        fields.push(SchemaField {
            path: path.clone(),
            schema: property,
            required: required.contains(name),
        });
        collect_schema_fields(root, property, &path, refs, depth + 1, fields);
        if let Some(items) = resolve_local_ref(root, property).get("items") {
            collect_schema_fields(root, items, &format!("{path}[]"), refs, depth + 1, fields);
        }
    }

    if let Some(reference) = reference {
        refs.remove(&reference);
    }
}

fn resolve_local_ref<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|reference| reference.strip_prefix('#'))
        .and_then(|pointer| root.pointer(pointer))
        .unwrap_or(schema)
}

fn schema_description<'a>(root: &'a Value, schema: &'a Value) -> Option<&'a str> {
    schema
        .get("description")
        .and_then(Value::as_str)
        .or_else(|| {
            resolve_local_ref(root, schema)
                .get("description")
                .and_then(Value::as_str)
        })
}

fn schema_type(root: &Value, schema: &Value) -> String {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str)
        && resolve_local_ref(root, schema) == schema
    {
        return reference
            .rsplit('/')
            .next()
            .unwrap_or(reference)
            .to_string();
    }
    let schema = resolve_local_ref(root, schema);
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .map(compact_json)
            .collect::<Vec<_>>()
            .join(" | ");
    }
    if let Some(types) = schema.get("type").and_then(Value::as_array) {
        return types
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" | ");
    }
    if let Some(schema_type_name) = schema.get("type").and_then(Value::as_str) {
        if schema_type_name == "array" {
            let item_type = schema
                .get("items")
                .map(|items| schema_type(root, items))
                .unwrap_or_else(|| "any".to_string());
            return format!("array<{item_type}>");
        }
        return schema_type_name.to_string();
    }
    if let Some(branches) = schema
        .get("oneOf")
        .or_else(|| schema.get("anyOf"))
        .and_then(Value::as_array)
    {
        return branches
            .iter()
            .map(|branch| schema_type(root, branch))
            .collect::<Vec<_>>()
            .join(" | ");
    }
    "any".to_string()
}

fn table_row(output: &mut String, field: &str, value: &str) {
    writeln!(output, "| {} | {} |", table_cell(field), value).unwrap();
}

fn table_cell(value: &str) -> String {
    escape_html(value)
        .replace('|', "\\|")
        .replace(['\r', '\n'], "<br>")
}

fn heading_text(value: &str) -> String {
    escape_html(value).replace(['\r', '\n'], " ")
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn inline_code(value: &str) -> String {
    let value = escape_html(value)
        .replace('|', "\\|")
        .replace(['\r', '\n'], " ");
    if !value.contains('`') {
        return format!("`{value}`");
    }
    let fence = "`".repeat(longest_backtick_run(&value) + 1);
    format!("{fence} {value} {fence}")
}

fn longest_backtick_run(value: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for character in value.chars() {
        if character == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

fn canonical_json(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonical_json).collect()),
        Value::Object(values) => {
            let mut entries: Vec<_> = values.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonical_json(value)))
                    .collect(),
            )
        }
        scalar => scalar,
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(&canonical_json(value.clone())).expect("JSON value is serializable")
}

fn nonempty(value: Option<&str>) -> bool {
    nonempty_value(value).is_some()
}

fn nonempty_value(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition<T: serde::de::DeserializeOwned>(value: Value) -> T {
        serde_json::from_value(value).unwrap()
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            protocol_version: "2025-11-25".to_string(),
            supported_versions: None,
            server_info: Implementation {
                name: "fixture".to_string(),
                version: "1.2.3".to_string(),
                description: Some("A fixture server.".to_string()),
                ..Default::default()
            },
            capabilities: ServerCapabilities::default(),
            instructions: None,
            tools: vec![definition(serde_json::json!({
                "name": "zeta",
                "description": "Search for a value.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "What to find."}
                    },
                    "required": ["query"]
                },
                "outputSchema": {
                    "type": "object",
                    "properties": {
                        "result": {"type": "string", "description": "The match."}
                    }
                }
            }))],
            prompts: Vec::new(),
            resources: Vec::new(),
            resource_templates: Vec::new(),
        }
    }

    #[test]
    fn assessment_counts_descriptions_but_not_optional_metadata() {
        let mut snapshot = snapshot();
        snapshot.tools[0].description = None;
        snapshot.tools[0].input_schema["properties"]["query"]
            .as_object_mut()
            .unwrap()
            .remove("description");

        let assessment = assess(&snapshot);

        assert_eq!(
            assessment.overall,
            Coverage {
                documented: 2,
                total: 4
            }
        );
        assert_eq!(assessment.overall.percentage(), 50);
        assert_eq!(assessment.gaps.len(), 2);
        assert_eq!(assessment.metadata.titled.total, 1);
        assert_eq!(assessment.metadata.titled.documented, 0);
        assert_eq!(assessment.metadata.tools_with_output_schema.documented, 1);
    }

    #[test]
    fn rendering_is_sorted_readable_and_lossless() {
        let mut snapshot = snapshot();
        snapshot.tools.push(definition(serde_json::json!({
            "name": "alpha",
            "description": "Runs first.",
            "inputSchema": {"type": "object", "properties": {}}
        })));
        snapshot.sort();

        let markdown = render_markdown(&snapshot, RenderOptions::default());

        assert!(markdown.find("`alpha`").unwrap() < markdown.find("`zeta`").unwrap());
        assert!(markdown.contains("| `query` | `string` | Yes | What to find. | — |"));
        assert!(markdown.contains("## Raw protocol inventory"));
        assert!(markdown.contains("\"inputSchema\""));
        assert!(markdown.contains("No tools or prompts were invoked"));
    }

    #[test]
    fn compact_render_omits_raw_json() {
        let markdown = render_markdown(
            &snapshot(),
            RenderOptions {
                assessment: true,
                raw_json: false,
            },
        );

        assert!(!markdown.contains("## Raw protocol inventory"));
        assert!(!markdown.contains("**Input schema**"));
        assert!(markdown.contains("**Parameters**"));
    }

    #[test]
    fn markdown_metacharacters_do_not_break_tables_or_code_spans() {
        assert_eq!(table_cell("a|b\nc"), "a\\|b<br>c");
        assert_eq!(inline_code("plain"), "`plain`");
        assert_eq!(inline_code("a`b"), "`` a`b ``");
        assert_eq!(heading_text("x<script>\ny"), "x&lt;script&gt; y");
    }

    #[test]
    fn assessment_walks_nested_object_fields() {
        let mut snapshot = snapshot();
        snapshot.tools[0].output_schema = None;
        snapshot.tools[0].input_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "object",
                    "description": "Search constraints.",
                    "properties": {
                        "limit": {"type": "integer"}
                    }
                }
            }
        });

        let assessment = assess(&snapshot);

        assert_eq!(
            assessment.overall,
            Coverage {
                documented: 3,
                total: 4
            }
        );
        assert_eq!(assessment.gaps.len(), 1);
        assert_eq!(assessment.gaps[0].path, "tools.zeta.input.filter.limit");
    }

    #[test]
    fn machine_report_includes_identity_score_and_gaps() {
        let mut snapshot = snapshot();
        snapshot.tools[0].description = None;

        let report = assessment_report(&snapshot);
        let json = serde_json::to_value(report).unwrap();

        assert_eq!(json["serverName"], "fixture");
        assert_eq!(json["protocolVersion"], "2025-11-25");
        assert_eq!(json["documentationScore"], 75);
        assert_eq!(json["assessment"]["gaps"][0]["path"], "tools.zeta");
    }
}
