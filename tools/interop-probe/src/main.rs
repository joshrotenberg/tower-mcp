//! Connects tower-mcp's client to real public MCP servers and reports what
//! happened, so interop breakage is noticed here rather than downstream.
//!
//! Every other test in this workspace runs against a server built from these
//! same types, which agrees with our assumptions by construction. A client's
//! actual job is coping with servers that were not. Both bugs this tool was
//! written after (#1212, #1213) were live while the client conformance suite
//! was green, because that suite drives our client against a *conformant*
//! peer and neither bug appears against one.
//!
//! ```bash
//! cargo run -p interop-probe                 # human-readable
//! cargo run -p interop-probe -- --snapshot   # diffable outcome record
//! ```
//!
//! # Read-only
//!
//! These are other people's services. The probe performs the handshake and
//! the list operations a server declares, and nothing else: no tool calls, no
//! resource reads, no prompt renders, no subscriptions. One pass per run.

use std::time::Duration;

use serde::Serialize;
use tower_mcp::client::{HttpClientTransport, McpClient};

/// A server reachable without credentials.
struct Target {
    name: &'static str,
    url: &'static str,
    /// What it is, for the human-readable output. Not diffed.
    note: &'static str,
}

const TARGETS: &[Target] = &[
    Target {
        name: "deepwiki",
        url: "https://mcp.deepwiki.com/mcp",
        note: "FastMCP; emits a `_fastmcp` _meta key (#1212)",
    },
    Target {
        name: "gitmcp",
        url: "https://gitmcp.io/docs",
        note: "declares tools only",
    },
    Target {
        name: "context7",
        url: "https://mcp.context7.com/mcp",
        note: "Zod-validated params; rejected a null cursor (#1213)",
    },
    Target {
        name: "huggingface",
        url: "https://huggingface.co/mcp",
        note: "@huggingface/mcp-services",
    },
];

/// Outcome of one operation. This is what gets diffed between runs, so it
/// deliberately excludes anything that changes for uninteresting reasons: no
/// counts, no timings, no timestamps. A server adding a tool must not look
/// like a regression, or the signal becomes noise and the lane stops being
/// trusted.
#[derive(Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "outcome")]
enum Outcome {
    /// The operation succeeded.
    Ok,
    /// The server does not declare this capability, so it was not attempted.
    NotDeclared,
    /// The operation failed. `error` is normalized: volatile substrings such
    /// as ports and request ids are stripped so the same failure compares
    /// equal across runs.
    Failed { error: String },
}

#[derive(Serialize)]
struct ServerReport {
    name: &'static str,
    connect: Outcome,
    protocol_version: Option<String>,
    tools: Outcome,
    resources: Outcome,
    resource_templates: Outcome,
    prompts: Outcome,
}

/// Detail for humans reading the run log, never part of the diffed snapshot.
struct Detail {
    server_name: Option<String>,
    counts: Vec<(&'static str, usize)>,
}

/// Strip the parts of an error that differ between runs of the same failure.
fn normalize(error: &tower_mcp::Error) -> String {
    let text = error.to_string();
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        // Collapse digit runs, which carry ports, ids, and durations.
        if c.is_ascii_digit() {
            while chars.peek().is_some_and(char::is_ascii_digit) {
                chars.next();
            }
            out.push('N');
            continue;
        }
        out.push(c);
    }
    // Keep it short enough to read in an issue body.
    out.chars().take(200).collect()
}

async fn probe(target: &Target) -> (ServerReport, Detail) {
    let mut report = ServerReport {
        name: target.name,
        connect: Outcome::Ok,
        protocol_version: None,
        tools: Outcome::NotDeclared,
        resources: Outcome::NotDeclared,
        resource_templates: Outcome::NotDeclared,
        prompts: Outcome::NotDeclared,
    };
    let mut detail = Detail {
        server_name: None,
        counts: Vec::new(),
    };

    let transport = HttpClientTransport::new(target.url);
    let client = match McpClient::connect(transport).await {
        Ok(client) => client,
        Err(error) => {
            report.connect = Outcome::Failed {
                error: normalize(&error),
            };
            return (report, detail);
        }
    };

    let initialized = match client
        .initialize("interop-probe", env!("CARGO_PKG_VERSION"))
        .await
    {
        Ok(result) => result,
        Err(error) => {
            report.connect = Outcome::Failed {
                error: normalize(&error),
            };
            return (report, detail);
        }
    };
    report.protocol_version = Some(initialized.protocol_version.clone());
    detail.server_name = Some(initialized.server_info.name.clone());

    // Only ask for what the server declares. Asking for an undeclared
    // capability earns a correct "method not found", which is the server
    // behaving properly rather than a finding.
    let caps = &initialized.capabilities;

    if caps.tools.is_some() {
        report.tools = match client.list_tools().await {
            Ok(result) => {
                detail.counts.push(("tools", result.tools.len()));
                Outcome::Ok
            }
            Err(error) => Outcome::Failed {
                error: normalize(&error),
            },
        };
    }
    if caps.resources.is_some() {
        report.resources = match client.list_resources().await {
            Ok(result) => {
                detail.counts.push(("resources", result.resources.len()));
                Outcome::Ok
            }
            Err(error) => Outcome::Failed {
                error: normalize(&error),
            },
        };
        report.resource_templates = match client.list_resource_templates().await {
            Ok(result) => {
                detail
                    .counts
                    .push(("resource_templates", result.resource_templates.len()));
                Outcome::Ok
            }
            Err(error) => Outcome::Failed {
                error: normalize(&error),
            },
        };
    }
    if caps.prompts.is_some() {
        report.prompts = match client.list_prompts().await {
            Ok(result) => {
                detail.counts.push(("prompts", result.prompts.len()));
                Outcome::Ok
            }
            Err(error) => Outcome::Failed {
                error: normalize(&error),
            },
        };
    }

    let _ = client.shutdown().await;
    (report, detail)
}

fn describe(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Ok => "ok".to_string(),
        Outcome::NotDeclared => "not declared".to_string(),
        Outcome::Failed { error } => format!("FAILED: {error}"),
    }
}

#[tokio::main]
async fn main() {
    let snapshot_mode = std::env::args().any(|a| a == "--snapshot");
    let mut reports = Vec::new();

    for target in TARGETS {
        // A third party being slow is not a finding, but it must not hang the
        // lane either.
        let probed = tokio::time::timeout(Duration::from_secs(45), probe(target)).await;
        let (report, detail) = match probed {
            Ok(pair) => pair,
            Err(_) => (
                ServerReport {
                    name: target.name,
                    connect: Outcome::Failed {
                        error: "timed out".to_string(),
                    },
                    protocol_version: None,
                    tools: Outcome::NotDeclared,
                    resources: Outcome::NotDeclared,
                    resource_templates: Outcome::NotDeclared,
                    prompts: Outcome::NotDeclared,
                },
                Detail {
                    server_name: None,
                    counts: Vec::new(),
                },
            ),
        };

        if !snapshot_mode {
            println!("## {} -- {}", target.name, target.note);
            println!("   url:      {}", target.url);
            if let Some(name) = &detail.server_name {
                println!("   server:   {name}");
            }
            if let Some(version) = &report.protocol_version {
                println!("   protocol: {version}");
            }
            println!("   connect:  {}", describe(&report.connect));
            for (label, outcome) in [
                ("tools", &report.tools),
                ("resources", &report.resources),
                ("templates", &report.resource_templates),
                ("prompts", &report.prompts),
            ] {
                println!("   {label:<9} {}", describe(outcome));
            }
            if !detail.counts.is_empty() {
                let counts: Vec<String> = detail
                    .counts
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                println!("   counts:   {}", counts.join(" "));
            }
            println!();
        }

        reports.push(report);
    }

    if snapshot_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&reports).expect("reports serialize")
        );
    } else {
        let failures = reports
            .iter()
            .filter(|r| {
                [
                    &r.connect,
                    &r.tools,
                    &r.resources,
                    &r.resource_templates,
                    &r.prompts,
                ]
                .iter()
                .any(|o| matches!(o, Outcome::Failed { .. }))
            })
            .count();
        println!(
            "{} of {} servers reported a failure",
            failures,
            reports.len()
        );
    }
}
