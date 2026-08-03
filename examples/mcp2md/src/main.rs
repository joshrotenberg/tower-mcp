//! mcp2md: generate Markdown documentation from an MCP server's discovered surface.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use mcp2md::{RenderOptions, Snapshot, assessment_report, render_markdown};
use tower_mcp::client::{
    HttpClientConfig, HttpClientTransport, McpClient, McpClientBuilder, StdioClientTransport,
};
use tower_mcp::protocol::{
    DiscoverResult, Implementation, InitializeResult, PromptDefinition, ResourceDefinition,
    ResourceTemplateDefinition, ServerCapabilities, ToolDefinition,
};
use tower_mcp::{ProtocolSupport, ProtocolSupportError};

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum ProtocolMode {
    #[default]
    Stable,
    #[value(name = "2026-07-28", alias = "final")]
    Final,
}

impl ProtocolMode {
    fn support(self) -> Result<ProtocolSupport, ProtocolSupportError> {
        match self {
            Self::Stable => Ok(ProtocolSupport::stable()),
            Self::Final => ProtocolSupport::try_new(["2026-07-28"]),
        }
    }
}

#[derive(Debug, Subcommand)]
enum Connection {
    /// Connect to a Streamable HTTP MCP endpoint.
    Http {
        /// MCP endpoint URL.
        url: String,

        /// Bearer token. MCP_BEARER is used when this flag is absent.
        #[arg(long)]
        bearer: Option<String>,

        /// Extra request header as `Name: Value`. Repeatable.
        #[arg(long = "header", value_name = "NAME: VALUE")]
        headers: Vec<String>,
    },

    /// Spawn a stdio MCP server as a child process.
    Stdio {
        /// Server command and arguments. Put `--` before the command when it
        /// has options that mcp2md should not parse.
        #[arg(required = true, trailing_var_arg = true, num_args = 1..)]
        command: Vec<String>,
    },
}

#[derive(Debug, Parser)]
#[command(
    name = "mcp2md",
    about = "Generate Markdown documentation from an MCP server",
    version
)]
struct Args {
    /// Runtime protocol lifecycle to inspect.
    #[arg(long, value_enum, default_value = "stable")]
    protocol: ProtocolMode,

    /// Write Markdown to this path instead of stdout. Use `-` for stdout.
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,

    /// Omit exact JSON schemas and the raw protocol inventory.
    #[arg(long)]
    compact: bool,

    /// Omit the documentation-coverage assessment.
    #[arg(long)]
    no_assessment: bool,

    /// Compare generated Markdown with this file and fail when it differs.
    #[arg(long, value_name = "PATH", conflicts_with = "output")]
    check: Option<PathBuf>,

    /// Also write the machine-readable documentation assessment as JSON.
    #[arg(long, value_name = "PATH")]
    assessment_output: Option<PathBuf>,

    /// Fail when documentation coverage is below this percentage.
    #[arg(long, value_name = "PERCENT", value_parser = clap::value_parser!(u8).range(0..=100))]
    fail_under: Option<u8>,

    #[command(subcommand)]
    connection: Connection,
}

#[derive(Debug)]
struct Handshake {
    protocol_version: String,
    supported_versions: Option<Vec<String>>,
    server_info: Implementation,
    capabilities: ServerCapabilities,
    instructions: Option<String>,
}

impl From<InitializeResult> for Handshake {
    fn from(result: InitializeResult) -> Self {
        Self {
            supported_versions: None,
            protocol_version: result.protocol_version,
            server_info: result.server_info,
            capabilities: result.capabilities,
            instructions: result.instructions,
        }
    }
}

impl Handshake {
    async fn from_discovery(client: &McpClient, result: DiscoverResult) -> Self {
        let server_info = result
            .meta
            .as_ref()
            .and_then(|meta| meta.server_info.clone())
            .unwrap_or_else(|| Implementation {
                name: "MCP server".to_string(),
                version: "unknown".to_string(),
                ..Default::default()
            });
        let protocol_version = client
            .selected_protocol_version()
            .await
            .unwrap_or_else(|| "2026-07-28".to_string());
        Self {
            protocol_version,
            supported_versions: Some(result.supported_versions),
            server_info,
            capabilities: result.capabilities,
            instructions: result.instructions,
        }
    }
}

#[tokio::main]
async fn main() -> Result<std::process::ExitCode, tower_mcp::BoxError> {
    let args = Args::parse();
    let client = connect(args.protocol, args.connection).await?;
    let handshake = establish(&client, args.protocol).await?;
    let mut snapshot = snapshot(&client, handshake).await?;
    snapshot.sort();
    client.shutdown().await?;

    let markdown = render_markdown(
        &snapshot,
        RenderOptions {
            assessment: !args.no_assessment,
            raw_json: !args.compact,
        },
    );
    let report = assessment_report(&snapshot);
    if let Some(path) = &args.assessment_output {
        write_assessment(path, &report)?;
    }

    let mut failed = false;
    if let Some(path) = &args.check {
        if !check_document(path, &markdown)? {
            eprintln!(
                "documentation is out of date: {} (regenerate with --output {})",
                path.display(),
                path.display()
            );
            failed = true;
        }
    } else {
        write_output(args.output.as_deref(), &markdown)?;
    }

    if let Some(minimum) = args.fail_under
        && report.documentation_score < usize::from(minimum)
    {
        eprintln!(
            "documentation coverage is {}%, below the required {}%",
            report.documentation_score, minimum
        );
        failed = true;
    }

    Ok(if failed {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    })
}

fn client_builder(protocol: ProtocolMode) -> Result<McpClientBuilder, ProtocolSupportError> {
    Ok(McpClient::builder().protocol_support(protocol.support()?))
}

async fn connect(
    protocol: ProtocolMode,
    connection: Connection,
) -> Result<McpClient, tower_mcp::BoxError> {
    let builder = client_builder(protocol)?;
    match connection {
        Connection::Http {
            url,
            bearer,
            headers,
        } => {
            let mut config = HttpClientConfig::default();
            if let Some(token) = bearer.or_else(|| std::env::var("MCP_BEARER").ok()) {
                config = config.bearer_token(token);
            }
            for header in headers {
                let (name, value) = parse_header(&header)?;
                config = config.header(name, value);
            }
            Ok(builder
                .connect_simple(HttpClientTransport::with_config(url, config))
                .await?)
        }
        Connection::Stdio { command } => {
            let arguments: Vec<_> = command[1..].iter().map(String::as_str).collect();
            let transport = StdioClientTransport::spawn(&command[0], &arguments).await?;
            Ok(builder.connect_simple(transport).await?)
        }
    }
}

async fn establish(
    client: &McpClient,
    protocol: ProtocolMode,
) -> Result<Handshake, tower_mcp::Error> {
    match protocol {
        ProtocolMode::Stable => Ok(client
            .initialize("mcp2md", env!("CARGO_PKG_VERSION"))
            .await?
            .into()),
        ProtocolMode::Final => {
            let discovery = client.discover("mcp2md", env!("CARGO_PKG_VERSION")).await?;
            Ok(Handshake::from_discovery(client, discovery).await)
        }
    }
}

async fn snapshot(client: &McpClient, handshake: Handshake) -> Result<Snapshot, tower_mcp::Error> {
    let list_tools = async {
        if handshake.capabilities.tools.is_some() {
            client.list_all_tools().await
        } else {
            Ok::<Vec<ToolDefinition>, tower_mcp::Error>(Vec::new())
        }
    };
    let list_prompts = async {
        if handshake.capabilities.prompts.is_some() {
            client.list_all_prompts().await
        } else {
            Ok::<Vec<PromptDefinition>, tower_mcp::Error>(Vec::new())
        }
    };
    let list_resources = async {
        if handshake.capabilities.resources.is_some() {
            client.list_all_resources().await
        } else {
            Ok::<Vec<ResourceDefinition>, tower_mcp::Error>(Vec::new())
        }
    };
    let list_templates = async {
        if handshake.capabilities.resources.is_some() {
            client.list_all_resource_templates().await
        } else {
            Ok::<Vec<ResourceTemplateDefinition>, tower_mcp::Error>(Vec::new())
        }
    };

    let (tools, prompts, resources, resource_templates) =
        tokio::join!(list_tools, list_prompts, list_resources, list_templates);
    let tools = list_result("tools/list", tools)?;
    let prompts = list_result("prompts/list", prompts)?;
    let resources = list_result("resources/list", resources)?;
    let resource_templates = list_result("resources/templates/list", resource_templates)?;

    Ok(Snapshot {
        protocol_version: handshake.protocol_version,
        supported_versions: handshake.supported_versions,
        server_info: handshake.server_info,
        capabilities: handshake.capabilities,
        instructions: handshake.instructions,
        tools,
        prompts,
        resources,
        resource_templates,
    })
}

fn list_result<T>(
    operation: &str,
    result: Result<Vec<T>, tower_mcp::Error>,
) -> Result<Vec<T>, tower_mcp::Error> {
    result.map_err(|error| {
        tower_mcp::Error::Transport(format!(
            "{operation} was advertised but could not be documented: {error}"
        ))
    })
}

fn parse_header(header: &str) -> Result<(&str, &str), std::io::Error> {
    let (name, value) = header.split_once(':').ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid --header {header:?}: expected `Name: Value`"),
        )
    })?;
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "header name cannot be empty",
        ));
    }
    Ok((name, value))
}

fn write_output(path: Option<&Path>, markdown: &str) -> std::io::Result<()> {
    match path {
        None => std::io::stdout().lock().write_all(markdown.as_bytes()),
        Some(path) if path == Path::new("-") => {
            std::io::stdout().lock().write_all(markdown.as_bytes())
        }
        Some(path) => std::fs::write(path, markdown),
    }
}

fn write_assessment(path: &Path, report: &mcp2md::AssessmentReport) -> std::io::Result<()> {
    let mut json = serde_json::to_string_pretty(report).map_err(std::io::Error::other)?;
    json.push('\n');
    std::fs::write(path, json)
}

fn check_document(path: &Path, generated: &str) -> std::io::Result<bool> {
    let existing = std::fs::read_to_string(path)?;
    if existing == generated {
        return Ok(true);
    }
    if let Some(line) = first_different_line(&existing, generated) {
        eprintln!("first difference is at line {line}");
    }
    Ok(false)
}

fn first_different_line(left: &str, right: &str) -> Option<usize> {
    let mut left = left.lines();
    let mut right = right.lines();
    for line in 1.. {
        match (left.next(), right.next()) {
            (None, None) => return None,
            (Some(left), Some(right)) if left == right => {}
            _ => return Some(line),
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headers_on_the_first_colon() {
        assert_eq!(
            parse_header("Authorization: Bearer a:b").unwrap(),
            ("Authorization", "Bearer a:b")
        );
    }

    #[test]
    fn rejects_malformed_headers() {
        assert!(parse_header("missing colon").is_err());
        assert!(parse_header(": value").is_err());
    }

    #[test]
    fn reports_the_first_different_line() {
        assert_eq!(first_different_line("a\nb\n", "a\nc\n"), Some(2));
        assert_eq!(first_different_line("a\n", "a\nb\n"), Some(2));
        assert_eq!(first_different_line("a\n", "a\n"), None);
    }

    #[test]
    fn output_and_check_are_mutually_exclusive() {
        let result = Args::try_parse_from([
            "mcp2md", "--output", "MCP.md", "--check", "MCP.md", "stdio", "server",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn fail_under_is_bounded_to_a_percentage() {
        assert!(Args::try_parse_from(["mcp2md", "--fail-under", "100", "stdio", "server"]).is_ok());
        assert!(
            Args::try_parse_from(["mcp2md", "--fail-under", "101", "stdio", "server"]).is_err()
        );
    }
}
